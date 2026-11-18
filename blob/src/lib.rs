//! Durable volume snapshots in object storage.
//!
//! A volume snapshot is stored as content-addressed chunks plus a manifest
//! (ADR 0011):
//!
//! ```text
//!   <prefix>/chunks/<h[0..2]>/<sha256>     zstd-compressed chunk data
//!   <prefix>/volume-snapshots/<id>.json    manifest, written last
//! ```
//!
//! Block images (microVM volumes) are cut into fixed 4 MiB chunks at aligned
//! offsets, and all-zero chunks are not stored, so an image keeps its
//! sparseness and a later snapshot of the same volume re-uses every block
//! that did not change. Directory volumes (container volumes) are written as
//! a tar stream in sorted order and cut with FastCDC, so an insertion moves
//! only the chunks around it. Chunks are named by the SHA-256 of their
//! uncompressed bytes: uploading one that already exists is skipped, and a
//! download that does not hash to its name is rejected.
//!
//! The manifest is uploaded after every chunk it names, so a manifest that
//! exists is complete. [`Remote::gc`] deletes chunks no manifest references.
//!
//! Any store [`object_store`] supports works: `s3://` (AWS and S3-compatible
//! stores such as MinIO, Ceph, R2), `az://` / `abfs://` (Azure Blob),
//! `gs://` (Google Cloud Storage), and `file://` (a local or shared
//! directory). Credentials come from the usual environment variables
//! (`AWS_*`, `AZURE_*`, `GOOGLE_*`) and instance or workload identity.

mod chunk;
mod error;
mod gc;
mod lazy;
mod lease;
mod manifest;
mod pull;
mod push;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};

pub use error::{Error, Result};
pub use gc::GcStats;
pub use lazy::{present_path, LazyImage};
pub use lease::{LeaseError, LeaseRecord, Version};
pub use manifest::{ChunkRef, Chunking, Kind, Manifest, SnapshotMeta, FORMAT};
pub use push::PushStats;

/// Environment variable prefixes forwarded to the store builders.
const ENV_PREFIXES: [&str; 4] = ["AWS_", "AZURE_", "GOOGLE_", "OBJECT_STORE_"];

/// A bucket (or container, or directory) holding volume snapshots.
#[derive(Clone)]
pub struct Remote {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    display: String,
    concurrency: usize,
    gc_grace: Duration,
    /// Where this handle's manifests live (see [`Remote::files_of`]).
    manifest_dir: String,
    /// A `file://` directory: no compare-and-swap, so leases and ownership
    /// records fall back to delete-then-create. Every other store must do
    /// real conditional writes or the write fails.
    local_dir: bool,
}

/// Root of the per-file manifests of sandbox (memory) snapshots.
pub const SANDBOX_FILES_DIR: &str = "sandbox-snapshot-files";

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("url", &self.display)
            .finish()
    }
}

impl Remote {
    /// Open a store from a URL such as `s3://bucket/prefix`. `options` are
    /// store settings (for example `endpoint`, `region`, `allow_http`,
    /// `use_emulator`); they override the environment.
    pub fn open(url: &str, options: &BTreeMap<String, String>) -> Result<Self> {
        let parsed = url::Url::parse(url).map_err(|e| Error::Config(format!("{url:?}: {e}")))?;
        let mut opts: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        opts.extend(
            options
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone())),
        );
        let (store, prefix) = object_store::parse_url_opts(&parsed, opts)?;
        let mut display = parsed.clone();
        let _ = display.set_username("");
        let _ = display.set_password(None);
        display.set_query(None);
        let mut remote =
            Self::from_store(Arc::from(store), prefix.as_ref()).with_display(display.to_string());
        remote.local_dir = parsed.scheme() == "file";
        Ok(remote)
    }

    /// Wrap an existing store; keys are placed under `prefix`.
    pub fn from_store(store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self {
            display: store.to_string(),
            store,
            prefix: prefix.trim_matches('/').to_string(),
            concurrency: 16,
            gc_grace: Duration::from_secs(24 * 3600),
            manifest_dir: manifest::MANIFEST_DIR.to_string(),
            local_dir: false,
        }
    }

    fn with_display(mut self, display: String) -> Self {
        self.display = display;
        self
    }

    /// Requests in flight per transfer (default 16).
    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// How old an unreferenced chunk must be before [`Remote::gc`] deletes
    /// it (default 24 h). Uploads re-write any existing chunk older than
    /// half of this, so a snapshot that finishes within that time can never
    /// lose a chunk to a concurrent collection.
    pub fn with_gc_grace(mut self, grace: Duration) -> Self {
        self.gc_grace = grace;
        self
    }

    pub fn gc_grace(&self) -> Duration {
        self.gc_grace
    }

    /// The store's URL without credentials, for logs and status output.
    pub fn describe(&self) -> &str {
        &self.display
    }

    fn key(&self, rel: &str) -> Path {
        if self.prefix.is_empty() {
            Path::from(rel)
        } else {
            Path::from(format!("{}/{rel}", self.prefix))
        }
    }

    /// A handle whose manifests are the files of sandbox snapshot `snapshot`
    /// (`<prefix>/sandbox-snapshot-files/<snapshot>/<file>.json`), sharing
    /// the same chunks. Push, pull, list and delete then work on those files.
    pub fn files_of(&self, snapshot: &str) -> Result<Self> {
        manifest::check_id(snapshot)?;
        let mut r = self.clone();
        r.manifest_dir = format!("{SANDBOX_FILES_DIR}/{snapshot}");
        Ok(r)
    }

    fn manifest_key(&self, id: &str) -> Result<Path> {
        manifest::check_id(id)?;
        Ok(self.key(&format!("{}/{id}.json", self.manifest_dir)))
    }

    /// The manifest of snapshot `id`.
    pub async fn manifest(&self, id: &str) -> Result<Manifest> {
        let key = self.manifest_key(id)?;
        let bytes = match self.store.get(&key).await {
            Ok(r) => r.bytes().await?,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(Error::NotFound(id.to_string()))
            }
            Err(e) => return Err(e.into()),
        };
        Manifest::parse(&bytes)
    }

    /// Every snapshot in the store, oldest first.
    pub async fn list(&self) -> Result<Vec<Manifest>> {
        self.list_under(&self.manifest_dir).await
    }

    pub(crate) async fn list_under(&self, dir: &str) -> Result<Vec<Manifest>> {
        let prefix = self.key(dir);
        let metas: Vec<_> = match self.store.list(Some(&prefix)).try_collect().await {
            Ok(m) => m,
            Err(object_store::Error::NotFound { .. }) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::with_capacity(metas.len());
        for meta in metas {
            if !meta.location.as_ref().ends_with(".json") {
                continue;
            }
            let bytes = self.store.get(&meta.location).await?.bytes().await?;
            out.push(Manifest::parse(&bytes)?);
        }
        out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(out)
    }

    /// Remove snapshot `id`. Its chunks stay until [`Remote::gc`].
    pub async fn delete(&self, id: &str) -> Result<()> {
        match self.store.delete(&self.manifest_key(id)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn put_manifest(&self, m: &Manifest) -> Result<()> {
        let key = self.manifest_key(&m.id)?;
        self.store.put(&key, m.to_json()?.into()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
