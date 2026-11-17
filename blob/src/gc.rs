use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use futures::TryStreamExt;
use object_store::ObjectStoreExt;

use crate::chunk::CHUNK_DIR;
use crate::{Remote, Result};

/// Result of a garbage collection pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    pub manifests: usize,
    pub chunks: usize,
    pub referenced: usize,
    /// Unreferenced but younger than the grace period: possibly part of an
    /// upload in progress, so kept.
    pub recent: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
}

impl Remote {
    /// Delete chunks that no manifest references and that are older than
    /// the grace period. With `dry_run`, only count them.
    ///
    /// Safe to run next to uploads: a manifest is written after its chunks,
    /// fresh chunks are protected by the grace period, and an upload renews
    /// any existing chunk older than half of it before relying on it.
    pub async fn gc(&self, dry_run: bool) -> Result<GcStats> {
        // Any unreadable manifest aborts the pass: deleting on partial
        // knowledge could destroy a snapshot.
        let manifests = self.list().await?;
        let referenced: HashSet<String> = manifests
            .iter()
            .flat_map(|m| m.chunks.iter().map(|c| c.hash.clone()))
            .collect();
        let cutoff = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .saturating_sub(self.gc_grace)
            .as_secs() as i64;
        let mut stats = GcStats {
            manifests: manifests.len(),
            referenced: referenced.len(),
            ..Default::default()
        };
        let prefix = self.key(CHUNK_DIR);
        let chunks: Vec<_> = match self.store.list(Some(&prefix)).try_collect().await {
            Ok(c) => c,
            Err(object_store::Error::NotFound { .. }) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        for meta in chunks {
            stats.chunks += 1;
            let Some(hash) = meta.location.filename() else {
                continue;
            };
            if referenced.contains(hash) {
                continue;
            }
            if meta.last_modified.timestamp() > cutoff {
                stats.recent += 1;
                continue;
            }
            if !dry_run {
                match self.store.delete(&meta.location).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            stats.deleted += 1;
            stats.deleted_bytes += meta.size;
        }
        tracing::info!(store = %self.display, dry_run, ?stats, "blob store garbage collection");
        Ok(stats)
    }
}
