//! Durable sandbox snapshots: memory, VM state and disks in the blob store
//! (decision 0018).
//!
//! A memory snapshot's files (Firecracker: `vmstate`, `mem`, `root.ext4`,
//! `meta.json`) are pushed as block images into the same content-addressed
//! chunk store as volumes, under `sandbox-snapshot-files/<id>/`; all-zero
//! memory is not stored. Its volume snapshots become durable volume
//! snapshots. A record at `sandbox-snapshots/<id>.json` holds the
//! `SnapshotRef` and the file list. A daemon asked to restore an id it does
//! not have downloads the files into its own snapshot directory and restores
//! as usual; the volumes restore lazily. Memory snapshots only restore on
//! the same Firecracker release and CPU model, which the backend checks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vmlayer_core::{Error, Result, SnapshotId, SnapshotRef};

use crate::manager::Daemon;

pub const SANDBOX_SNAPSHOTS_DIR: &str = "sandbox-snapshots";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileEntry {
    /// File name in the snapshot directory.
    name: String,
    /// Manifest id in the store.
    id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    snapshot: SnapshotRef,
    files: Vec<FileEntry>,
}

fn blob(e: vmlayer_blob::Error) -> Error {
    Error::Engine {
        backend: "blob-store",
        message: e.to_string(),
        source: None,
    }
}

fn file_id(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Daemon {
    fn remote_or_err(&self) -> Result<&vmlayer_blob::Remote> {
        self.volumes.remote().ok_or_else(|| {
            Error::Guest(
                "no blob store is configured (set [blob_store] url or --blob-store)".into(),
            )
        })
    }

    /// Where a backend keeps its snapshot directories on this host.
    fn snapshot_home(&self, backend: &str, id: &SnapshotId) -> PathBuf {
        self.cfg
            .data_dir
            .join(backend)
            .join("snapshots")
            .join(&id.0)
    }

    /// Upload a just-taken snapshot's files and record it.
    pub(crate) async fn push_sandbox_snapshot(&self, snap: &SnapshotRef) -> Result<()> {
        let remote = self.remote_or_err()?;
        let dir = Path::new(&snap.engine_ref);
        if !dir.is_dir() || !dir.starts_with(&self.cfg.data_dir) {
            return Err(Error::Unsupported {
                backend: "daemon",
                feature: "durable sandbox snapshots on this backend (use a microVM backend)",
            });
        }
        let files_remote = remote.files_of(&snap.id.0).map_err(blob)?;
        let mut files = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let id = file_id(&name);
            let meta = vmlayer_blob::SnapshotMeta {
                id: id.clone(),
                volume_id: snap.id.0.clone(),
                ..Default::default()
            };
            files_remote
                .push_block(&entry.path(), &meta)
                .await
                .map_err(blob)?;
            files.push(FileEntry { name, id });
        }
        let record = Record {
            snapshot: snap.clone(),
            files,
        };
        let bytes = serde_json::to_vec(&record).map_err(|e| Error::Guest(e.to_string()))?;
        remote
            .put_doc(SANDBOX_SNAPSHOTS_DIR, &snap.id.0, &bytes)
            .await
            .map_err(blob)?;
        tracing::info!(snapshot = %snap.id, files = record.files.len(), "sandbox snapshot pushed");
        Ok(())
    }

    async fn record(&self, id: &SnapshotId) -> Result<Option<Record>> {
        let remote = self.remote_or_err()?;
        let doc = remote
            .get_doc(SANDBOX_SNAPSHOTS_DIR, &id.0)
            .await
            .map_err(blob)?;
        doc.map(|b| {
            serde_json::from_slice(&b).map_err(|e| Error::Guest(format!("snapshot record: {e}")))
        })
        .transpose()
    }

    /// Download a snapshot pushed by any daemon sharing the store.
    pub(crate) async fn import_sandbox_snapshot(&self, id: &SnapshotId) -> Result<SnapshotRef> {
        let remote = self.remote_or_err()?;
        let Some(record) = self.record(id).await? else {
            return Err(Error::SnapshotNotFound(id.clone()));
        };
        let mut snap = record.snapshot;
        if self.registry.get(&snap.backend).is_none() {
            return Err(Error::SnapshotIncompatible {
                id: id.clone(),
                reason: format!("backend {} not available here", snap.backend),
            });
        }
        let dir = self.snapshot_home(&snap.backend, id);
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let files_remote = remote.files_of(&id.0).map_err(blob)?;
        for f in &record.files {
            let pulled = async {
                let manifest = files_remote.manifest(&f.id).await?;
                files_remote.pull(&manifest, &dir.join(&f.name)).await
            };
            if let Err(e) = pulled.await {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(blob(e));
            }
        }
        snap.engine_ref = dir.display().to_string();
        self.store
            .insert_snapshot(&snap)
            .map_err(|e| Error::Guest(e.to_string()))?;
        tracing::info!(snapshot = %id, "sandbox snapshot imported from the blob store");
        Ok(snap)
    }

    /// Remove a durable snapshot from the store.
    pub(crate) async fn delete_remote_sandbox_snapshot(&self, id: &SnapshotId) -> Result<()> {
        let remote = self.remote_or_err()?;
        if let Some(record) = self.record(id).await? {
            let files_remote = remote.files_of(&id.0).map_err(blob)?;
            for f in &record.files {
                files_remote.delete(&f.id).await.map_err(blob)?;
            }
            for vs in &record.snapshot.volumes {
                let _ = self.volumes.delete_snapshot(&vs.id, false).await;
            }
        }
        remote
            .delete_doc(SANDBOX_SNAPSHOTS_DIR, &id.0)
            .await
            .map_err(blob)
    }

    /// Durable sandbox snapshots in the store, from any daemon.
    pub async fn list_remote_snapshots(&self) -> Result<Vec<SnapshotRef>> {
        let remote = self.remote_or_err()?;
        let docs = remote
            .list_docs(SANDBOX_SNAPSHOTS_DIR)
            .await
            .map_err(blob)?;
        Ok(docs
            .iter()
            .filter_map(|b| serde_json::from_slice::<Record>(b).ok())
            .map(|r| r.snapshot)
            .collect())
    }
}
