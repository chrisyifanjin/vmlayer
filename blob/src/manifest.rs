use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Format tag written into every manifest. A reader refuses other formats.
pub const FORMAT: &str = "vmlayer.volume-snapshot/1";
pub(crate) const MANIFEST_DIR: &str = "volume-snapshots";

/// Representation of the volume the snapshot came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An ext4 image attached as a block device (microVM backends).
    Block,
    /// A directory bind-mounted into a container.
    Dir,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Chunking {
    /// Aligned chunks of `size` bytes; all-zero chunks are omitted.
    Fixed { size: u64 },
    /// FastCDC (2020) over a tar stream of the directory.
    Fastcdc2020 { min: u32, avg: u32, max: u32 },
}

/// One chunk: `length` bytes at `offset` of the image or tar stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub offset: u64,
    pub length: u64,
    /// Hex SHA-256 of the uncompressed bytes; also the object name.
    pub hash: String,
}

/// What the caller knows about the snapshot being pushed.
#[derive(Clone, Debug, Default)]
pub struct SnapshotMeta {
    pub id: String,
    pub volume_id: String,
    pub size_mib: u64,
    pub guest_path: Option<String>,
    pub labels: BTreeMap<String, String>,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub id: String,
    pub volume_id: String,
    pub kind: Kind,
    /// Size of the volume it came from.
    pub size_mib: u64,
    /// Bytes in the image (block) or tar stream (dir).
    pub logical_size: u64,
    pub chunking: Chunking,
    pub compression: String,
    pub chunks: Vec<ChunkRef>,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_path: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Manifest {
    pub(crate) fn new(
        meta: &SnapshotMeta,
        kind: Kind,
        chunking: Chunking,
        logical_size: u64,
        chunks: Vec<ChunkRef>,
    ) -> Self {
        Self {
            format: FORMAT.into(),
            id: meta.id.clone(),
            volume_id: meta.volume_id.clone(),
            kind,
            size_mib: meta.size_mib,
            logical_size,
            chunking,
            compression: "zstd".into(),
            chunks,
            created_at: meta.created_at,
            guest_path: meta.guest_path.clone(),
            labels: meta.labels.clone(),
        }
    }

    /// Bytes actually stored in chunks, before compression.
    pub fn stored_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.length).sum()
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| Error::Manifest(e.to_string()))?;
        if m.format != FORMAT {
            return Err(Error::Manifest(format!(
                "unsupported format {:?} (this build reads {FORMAT:?})",
                m.format
            )));
        }
        if m.compression != "zstd" {
            return Err(Error::Manifest(format!(
                "unsupported compression {:?}",
                m.compression
            )));
        }
        check_id(&m.id)?;
        for c in &m.chunks {
            if c.hash.len() != 64 || !c.hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Manifest(format!("bad chunk hash {:?}", c.hash)));
            }
            if c.offset
                .checked_add(c.length)
                .is_none_or(|end| end > m.logical_size)
            {
                return Err(Error::Manifest(format!(
                    "chunk at {} (+{}) lies outside the {} byte snapshot",
                    c.offset, c.length, m.logical_size
                )));
            }
        }
        if m.kind == Kind::Dir {
            // A tar stream is rebuilt by concatenation: it must be gapless.
            let mut next = 0;
            for c in &m.chunks {
                if c.offset != next {
                    return Err(Error::Manifest("directory snapshot has a gap".into()));
                }
                next += c.length;
            }
            if next != m.logical_size {
                return Err(Error::Manifest("directory snapshot is truncated".into()));
            }
        }
        Ok(m)
    }

    pub(crate) fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|e| Error::Manifest(e.to_string()))
    }
}

/// Snapshot ids become object names: keep them to a safe alphabet.
pub(crate) fn check_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Manifest(format!("invalid snapshot id {id:?}")))
    }
}
