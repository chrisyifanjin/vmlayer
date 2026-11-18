use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use tokio_stream::wrappers::ReceiverStream;

use crate::chunk::{BLOCK_CHUNK, CDC_AVG, CDC_MAX, CDC_MIN};
use crate::{ChunkRef, Chunking, Error, Kind, Manifest, Remote, Result, SnapshotMeta};

/// What a push sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushStats {
    /// Chunks the snapshot references.
    pub chunks: usize,
    /// Of those, chunks that had to be uploaded.
    pub uploaded_chunks: usize,
    /// Compressed bytes uploaded.
    pub uploaded_bytes: u64,
    /// Bytes in the image or tar stream, before dropping zero chunks.
    pub logical_bytes: u64,
}

impl Remote {
    /// Upload a block image (a raw ext4 file). Returns the manifest written.
    pub async fn push_block(
        &self,
        image: &Path,
        meta: &SnapshotMeta,
    ) -> Result<(Manifest, PushStats)> {
        let file = Arc::new(std::fs::File::open(image)?);
        let len = file.metadata()?.len();
        let offsets: Vec<u64> = (0..len).step_by(BLOCK_CHUNK as usize).collect();
        let this = self.clone();
        let stored: Vec<_> = futures::stream::iter(offsets)
            .map(move |offset| {
                let file = file.clone();
                let this = this.clone();
                async move {
                    let n = BLOCK_CHUNK.min(len - offset) as usize;
                    let data = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                        let mut buf = vec![0u8; n];
                        file.read_exact_at(&mut buf, offset)?;
                        Ok(buf)
                    })
                    .await
                    .map_err(std::io::Error::other)??;
                    if is_zero(&data) {
                        return Ok(None);
                    }
                    this.put_chunk(offset, data).await.map(Some)
                }
            })
            .buffered(self.concurrency)
            .try_filter_map(|s| async move { Ok(s) })
            .try_collect()
            .await?;
        self.finish(
            meta,
            Kind::Block,
            Chunking::Fixed { size: BLOCK_CHUNK },
            len,
            stored,
        )
        .await
    }

    /// Upload a directory as a sorted tar stream cut by FastCDC.
    pub async fn push_dir(&self, dir: &Path, meta: &SnapshotMeta) -> Result<(Manifest, PushStats)> {
        let (tx, rx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>)>(self.concurrency);
        let root = dir.to_path_buf();
        let producer = tokio::task::spawn_blocking(move || -> Result<u64> {
            let (reader, writer) = std::io::pipe()?;
            let tar = std::thread::spawn(move || write_tar(&root, writer));
            let mut total = 0u64;
            let mut consumer_gone = false;
            for chunk in fastcdc::v2020::StreamCDC::new(
                reader,
                CDC_MIN as usize,
                CDC_AVG as usize,
                CDC_MAX as usize,
            ) {
                let chunk = chunk.map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
                total += chunk.length as u64;
                if tx.blocking_send((chunk.offset, chunk.data)).is_err() {
                    consumer_gone = true;
                    break;
                }
            }
            let written = tar
                .join()
                .map_err(|_| std::io::Error::other("tar writer panicked"))?;
            if consumer_gone {
                return Ok(total);
            }
            written?;
            Ok(total)
        });
        let this = self.clone();
        let uploaded: Result<Vec<_>> = ReceiverStream::new(rx)
            .map(move |(offset, data)| {
                let this = this.clone();
                async move { this.put_chunk(offset, data).await }
            })
            .buffered(self.concurrency)
            .try_collect()
            .await;
        let total = producer.await.map_err(std::io::Error::other)??;
        let stored = uploaded?;
        self.finish(
            meta,
            Kind::Dir,
            Chunking::Fastcdc2020 {
                min: CDC_MIN,
                avg: CDC_AVG,
                max: CDC_MAX,
            },
            total,
            stored,
        )
        .await
    }

    async fn finish(
        &self,
        meta: &SnapshotMeta,
        kind: Kind,
        chunking: Chunking,
        logical: u64,
        stored: Vec<crate::chunk::Stored>,
    ) -> Result<(Manifest, PushStats)> {
        let stats = PushStats {
            chunks: stored.len(),
            uploaded_chunks: stored.iter().filter(|s| s.uploaded > 0).count(),
            uploaded_bytes: stored.iter().map(|s| s.uploaded).sum(),
            logical_bytes: logical,
        };
        let reused: Vec<ChunkRef> = stored
            .iter()
            .filter(|s| s.uploaded == 0)
            .map(|s| s.chunk.clone())
            .collect();
        let chunks = stored.into_iter().map(|s| s.chunk).collect();
        let manifest = Manifest::new(meta, kind, chunking, logical, chunks);
        // Last: a manifest in the store implies all of its chunks are there.
        self.put_manifest(&manifest).await?;
        // A chunk reused (not uploaded) may have been collected between our
        // check and the manifest write. Verify, and withdraw if one is gone.
        if let Some(gone) = self.missing_chunks(&reused).await?.into_iter().next() {
            let _ = self.delete(&manifest.id).await;
            return Err(Error::Raced(gone));
        }
        tracing::info!(
            snapshot = %manifest.id,
            store = %self.display,
            chunks = stats.chunks,
            uploaded_chunks = stats.uploaded_chunks,
            uploaded_bytes = stats.uploaded_bytes,
            "volume snapshot pushed"
        );
        Ok((manifest, stats))
    }
}

fn is_zero(data: &[u8]) -> bool {
    let (head, words, tail) = unsafe { data.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && words.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// Deterministic tar of `root`: entries sorted by name, full metadata,
/// symlinks stored as links. Sockets, FIFOs and devices are skipped.
fn write_tar(root: &Path, out: impl Write) -> Result<()> {
    let mut builder = tar::Builder::new(out);
    builder.mode(tar::HeaderMode::Complete);
    builder.follow_symlinks(false);
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut entries: Vec<_> =
            std::fs::read_dir(root.join(&rel))?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let name = rel.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                builder.append_path_with_name(entry.path(), &name)?;
                subdirs.push(name);
            } else if kind.is_file() || kind.is_symlink() {
                builder.append_path_with_name(entry.path(), &name)?;
            } else {
                tracing::debug!(path = %name.display(), "skipping special file in volume snapshot");
            }
        }
        // Depth first, in name order.
        stack.extend(subdirs.into_iter().rev());
    }
    builder.into_inner()?.flush()?;
    Ok(())
}
