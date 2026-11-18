use std::time::{Duration, SystemTime};

use object_store::{ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};

use crate::{ChunkRef, Error, Remote, Result};

/// Fixed chunk size for block images. 4 MiB matches Proxmox Backup Server's
/// fixed index and keeps per-request overhead small on object stores.
pub(crate) const BLOCK_CHUNK: u64 = 4 << 20;
/// FastCDC bounds for directory streams (average 4 MiB, like the above).
pub(crate) const CDC_MIN: u32 = 1 << 20;
pub(crate) const CDC_AVG: u32 = 4 << 20;
pub(crate) const CDC_MAX: u32 = 16 << 20;
pub(crate) const CHUNK_DIR: &str = "chunks";
const ZSTD_LEVEL: i32 = 3;

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub(crate) fn chunk_rel(hash: &str) -> String {
    format!("{CHUNK_DIR}/{}/{hash}", &hash[..2])
}

/// Outcome of storing one chunk.
pub(crate) struct Stored {
    pub(crate) chunk: ChunkRef,
    /// Compressed bytes sent, 0 when the chunk was already present.
    pub(crate) uploaded: u64,
}

impl Remote {
    /// Store `data` under its hash unless a recent copy is already there.
    pub(crate) async fn put_chunk(&self, offset: u64, data: Vec<u8>) -> Result<Stored> {
        let hash = sha256_hex(&data);
        let key = self.key(&chunk_rel(&hash));
        let chunk = ChunkRef {
            offset,
            length: data.len() as u64,
            hash,
        };
        match self.store.head(&key).await {
            Ok(meta) if !self.needs_refresh(meta.last_modified.timestamp()) => {
                return Ok(Stored { chunk, uploaded: 0 })
            }
            Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        let compressed =
            tokio::task::spawn_blocking(move || zstd::bulk::compress(&data, ZSTD_LEVEL))
                .await
                .map_err(std::io::Error::other)??;
        let uploaded = compressed.len() as u64;
        self.store.put(&key, PutPayload::from(compressed)).await?;
        Ok(Stored { chunk, uploaded })
    }

    /// An existing chunk older than half the GC grace period is written
    /// again, which renews its modification time (see `with_gc_grace`).
    fn needs_refresh(&self, modified_unix: i64) -> bool {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs() as i64;
        now - modified_unix > (self.gc_grace / 2).as_secs() as i64
    }

    /// Hashes of `chunks` that are not in the store.
    pub(crate) async fn missing_chunks(&self, chunks: &[ChunkRef]) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for c in chunks {
            match self.store.head(&self.key(&chunk_rel(&c.hash))).await {
                Ok(_) => {}
                Err(object_store::Error::NotFound { .. }) => missing.push(c.hash.clone()),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(missing)
    }

    /// Download, decompress and verify one chunk.
    pub(crate) async fn get_chunk(&self, chunk: &ChunkRef) -> Result<Vec<u8>> {
        let key = self.key(&chunk_rel(&chunk.hash));
        let compressed = match self.store.get(&key).await {
            Ok(r) => r.bytes().await?,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(Error::Corrupt {
                    hash: chunk.hash.clone(),
                    reason: "missing from the store".into(),
                })
            }
            Err(e) => return Err(e.into()),
        };
        let chunk = chunk.clone();
        tokio::task::spawn_blocking(move || {
            let data = zstd::bulk::decompress(&compressed, chunk.length as usize).map_err(|e| {
                Error::Corrupt {
                    hash: chunk.hash.clone(),
                    reason: e.to_string(),
                }
            })?;
            if data.len() as u64 != chunk.length || sha256_hex(&data) != chunk.hash {
                return Err(Error::Corrupt {
                    hash: chunk.hash,
                    reason: "content does not match its hash".into(),
                });
            }
            Ok(data)
        })
        .await
        .map_err(std::io::Error::other)?
    }
}
