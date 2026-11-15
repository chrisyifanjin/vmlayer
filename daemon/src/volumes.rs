//! The volume store: creates, clones and snapshots volume data on the host.
//!
//! v1 keeps every volume as a directory under `<data_dir>/volumes/<id>` and
//! every snapshot under `<data_dir>/volume-snapshots/<id>`. Clone and
//! snapshot are copy-on-write where the filesystem allows it (`cp
//! --reflink=auto` on Linux, `cp -c` on APFS) and plain copies otherwise.
//! Block-image volumes for microVM backends will add a second
//! representation behind the same methods (ADR 0006 D11).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::process::Command;
use uuid::Uuid;
use vmlayer_core::{Error, Result, VolumeId, VolumeSnapshotId, VolumeSource, VolumeSourceKind};

use crate::store::{Store, StoreError, VolumeRow, VolumeSnapshotRow};

#[derive(Clone)]
pub struct VolumeStore {
    root: PathBuf,
    store: Store,
    default_size_mib: u64,
}

fn store_err(e: StoreError) -> Error {
    match e {
        StoreError::NotFound => Error::Guest("not found".into()),
        other => Error::Engine {
            backend: "daemon",
            message: other.to_string(),
            source: Some(Box::new(other)),
        },
    }
}

/// File name of the ext4 image inside a block-image volume's directory.
pub const IMAGE_FILE: &str = "image.ext4";

/// Sparse ext4 image: costs nothing until written.
async fn make_ext4_image(path: &Path, size_mib: u64) -> Result<()> {
    let file = tokio::fs::File::create(path).await?;
    file.set_len(size_mib * 1024 * 1024).await?;
    drop(file);
    let out = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-E", "lazy_itable_init=1,lazy_journal_init=1"])
        .arg(path)
        .output()
        .await
        .map_err(|e| Error::Engine {
            backend: "daemon",
            message: format!("mkfs.ext4: {e} (install e2fsprogs for block-image volumes)"),
            source: None,
        })?;
    if !out.status.success() {
        return Err(Error::Engine {
            backend: "daemon",
            message: format!(
                "mkfs.ext4 {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            source: None,
        });
    }
    Ok(())
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::now_v7().simple())
}

/// Copy a directory tree, copy-on-write when the platform supports it.
pub async fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut cmd = Command::new("cp");
    #[cfg(target_os = "linux")]
    cmd.args(["-a", "--reflink=auto"]);
    #[cfg(target_os = "macos")]
    cmd.args(["-c", "-R", "-p"]);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    cmd.args(["-R", "-p"]);
    // Trailing `/.` copies the contents, not the directory itself.
    cmd.arg(format!("{}/.", src.display())).arg(dst);
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(Error::Engine {
            backend: "daemon",
            message: format!(
                "copying {} to {}: {}",
                src.display(),
                dst.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            source: None,
        });
    }
    Ok(())
}

async fn tree_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

impl VolumeStore {
    pub fn new(data_dir: &Path, store: Store, default_size_mib: u64) -> Self {
        Self {
            root: data_dir.to_path_buf(),
            store,
            default_size_mib,
        }
    }

    fn volume_dir(&self, id: &VolumeId) -> PathBuf {
        self.root.join("volumes").join(&id.0)
    }

    fn snapshot_dir(&self, id: &VolumeSnapshotId) -> PathBuf {
        self.root.join("volume-snapshots").join(&id.0)
    }

    /// Create a volume in the representation `kind` asks for. A directory
    /// volume is just a directory; a block-image volume holds a sparse ext4
    /// image (`image.ext4`) formatted with `mkfs.ext4`, which microVM
    /// backends attach as a virtio-blk device.
    pub async fn create_kind(
        &self,
        kind: VolumeSourceKind,
        name: Option<String>,
        size_mib: Option<u64>,
        labels: BTreeMap<String, String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        let id = VolumeId(new_id("vol"));
        let path = self.volume_dir(&id);
        tokio::fs::create_dir_all(&path).await?;
        let size = size_mib.unwrap_or(self.default_size_mib);
        if kind == VolumeSourceKind::BlockImage {
            if let Err(e) = make_ext4_image(&path.join(IMAGE_FILE), size).await {
                let _ = tokio::fs::remove_dir_all(&path).await;
                return Err(e);
            }
        }
        let row = VolumeRow {
            id,
            name,
            size_mib: size,
            labels,
            anonymous,
            attached_to: None,
            created_at: SystemTime::now(),
            path,
        };
        self.store.insert_volume(&row).map_err(|e| match e {
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Error::Guest(format!("volume name {:?} is taken", row.name))
            }
            e => store_err(e),
        })?;
        Ok(row)
    }

    /// Directory volume (the Docker backend's representation).
    pub async fn create(
        &self,
        name: Option<String>,
        size_mib: Option<u64>,
        labels: BTreeMap<String, String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        self.create_kind(VolumeSourceKind::Dir, name, size_mib, labels, anonymous)
            .await
    }

    pub fn get(&self, id_or_name: &str) -> Result<VolumeRow> {
        self.store.get_volume(id_or_name).map_err(|e| match e {
            StoreError::NotFound => Error::VolumeNotFound(VolumeId(id_or_name.to_string())),
            e => store_err(e),
        })
    }

    pub fn list(&self) -> Result<Vec<VolumeRow>> {
        self.store.list_volumes().map_err(store_err)
    }

    /// The representation a volume has: block image if it holds one, else a
    /// directory. Fixed at creation.
    pub fn kind(&self, row: &VolumeRow) -> VolumeSourceKind {
        if row.path.join(IMAGE_FILE).is_file() {
            VolumeSourceKind::BlockImage
        } else {
            VolumeSourceKind::Dir
        }
    }

    pub fn source(&self, row: &VolumeRow) -> VolumeSource {
        match self.kind(row) {
            VolumeSourceKind::BlockImage => VolumeSource::BlockImage {
                path: row.path.join(IMAGE_FILE),
            },
            VolumeSourceKind::Dir => VolumeSource::Dir {
                path: row.path.clone(),
            },
        }
    }

    /// Refuse to hand a volume to a backend that cannot mount its representation.
    pub fn check_kind(&self, row: &VolumeRow, wanted: VolumeSourceKind) -> Result<()> {
        let have = self.kind(row);
        if have == wanted {
            Ok(())
        } else {
            Err(Error::Guest(format!(
                "volume {} is a {:?} volume but this backend needs {:?} volumes; create it with the right backend",
                row.id, have, wanted
            )))
        }
    }

    /// Enforce the single-writer rule and record the attachment.
    pub fn attach(
        &self,
        volume: &VolumeId,
        sandbox: &vmlayer_core::SandboxId,
        read_only: bool,
    ) -> Result<()> {
        if read_only {
            return Ok(());
        }
        let row = self.get(&volume.0)?;
        if let Some(holder) = row.attached_to {
            if &holder != sandbox {
                return Err(Error::VolumeBusy {
                    volume: volume.clone(),
                    holder,
                });
            }
        }
        self.store
            .set_volume_attachment(volume, Some(sandbox))
            .map_err(store_err)
    }

    pub fn detach_all(&self, sandbox: &vmlayer_core::SandboxId) -> Result<()> {
        for v in self.list()? {
            if v.attached_to.as_ref() == Some(sandbox) {
                self.store
                    .set_volume_attachment(&v.id, None)
                    .map_err(store_err)?;
            }
        }
        Ok(())
    }

    pub async fn delete(&self, id: &VolumeId) -> Result<()> {
        let row = self.get(&id.0)?;
        if let Some(holder) = row.attached_to {
            return Err(Error::VolumeBusy {
                volume: id.clone(),
                holder,
            });
        }
        self.store.delete_volume(id).map_err(store_err)?;
        if row.path.starts_with(&self.root) {
            let _ = tokio::fs::remove_dir_all(&row.path).await;
        }
        Ok(())
    }

    pub async fn snapshot(
        &self,
        id: &VolumeId,
        guest_path: Option<String>,
        labels: BTreeMap<String, String>,
    ) -> Result<VolumeSnapshotRow> {
        let row = self.get(&id.0)?;
        let snap_id = VolumeSnapshotId(new_id("vsnap"));
        let path = self.snapshot_dir(&snap_id);
        clone_tree(&row.path, &path).await?;
        let size_bytes = tree_size(&path).await;
        let snap = VolumeSnapshotRow {
            id: snap_id,
            volume_id: id.clone(),
            guest_path,
            created_at: SystemTime::now(),
            size_bytes,
            // No blob store in v1: local only.
            durable: false,
            labels,
            path,
        };
        self.store
            .insert_volume_snapshot(&snap)
            .map_err(store_err)?;
        Ok(snap)
    }

    pub fn get_snapshot(&self, id: &VolumeSnapshotId) -> Result<VolumeSnapshotRow> {
        self.store.get_volume_snapshot(id).map_err(|e| match e {
            StoreError::NotFound => Error::Guest(format!("volume snapshot {id} not found")),
            e => store_err(e),
        })
    }

    pub fn list_snapshots(&self) -> Result<Vec<VolumeSnapshotRow>> {
        self.store.list_volume_snapshots().map_err(store_err)
    }

    pub async fn delete_snapshot(&self, id: &VolumeSnapshotId) -> Result<()> {
        let row = self.get_snapshot(id)?;
        self.store.delete_volume_snapshot(id).map_err(store_err)?;
        if row.path.starts_with(&self.root) {
            let _ = tokio::fs::remove_dir_all(&row.path).await;
        }
        Ok(())
    }

    /// A new volume whose contents are a clone of a snapshot.
    pub async fn from_snapshot(
        &self,
        snapshot: &VolumeSnapshotId,
        name: Option<String>,
        labels: BTreeMap<String, String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        let snap = self.get_snapshot(snapshot)?;
        let src_vol = self.get(&snap.volume_id.0).ok();
        let row = self
            .create(name, src_vol.map(|v| v.size_mib), labels, anonymous)
            .await?;
        clone_tree(&snap.path, &row.path).await?;
        Ok(row)
    }

    /// A new volume whose contents are a clone of another volume's current state.
    pub async fn clone_volume(
        &self,
        id: &VolumeId,
        name: Option<String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        let src = self.get(&id.0)?;
        let row = self
            .create(name, Some(src.size_mib), src.labels.clone(), anonymous)
            .await?;
        clone_tree(&src.path, &row.path).await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_snapshot_restore_clone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let vs = VolumeStore::new(dir.path(), store, 100);
        let v = vs
            .create(Some("a".into()), None, BTreeMap::new(), false)
            .await
            .unwrap();
        assert_eq!(v.size_mib, 100);
        tokio::fs::write(v.path.join("f.txt"), b"one")
            .await
            .unwrap();

        let snap = vs
            .snapshot(&v.id, Some("/work".into()), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(snap.size_bytes, 3);
        tokio::fs::write(v.path.join("f.txt"), b"two")
            .await
            .unwrap();

        let restored = vs
            .from_snapshot(&snap.id, None, BTreeMap::new(), true)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(restored.path.join("f.txt")).await.unwrap(),
            b"one"
        );
        let cloned = vs
            .clone_volume(&v.id, Some("b".into()), false)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(cloned.path.join("f.txt")).await.unwrap(),
            b"two"
        );

        // single writer
        let sb = vmlayer_core::SandboxId("sb1".into());
        vs.attach(&v.id, &sb, false).unwrap();
        vs.attach(&v.id, &sb, false).unwrap(); // same holder is fine
        let other = vmlayer_core::SandboxId("sb2".into());
        assert!(matches!(
            vs.attach(&v.id, &other, false),
            Err(Error::VolumeBusy { .. })
        ));
        vs.attach(&v.id, &other, true).unwrap(); // read-only is fine
        assert!(matches!(
            vs.delete(&v.id).await,
            Err(Error::VolumeBusy { .. })
        ));
        vs.detach_all(&sb).unwrap();
        vs.delete(&v.id).await.unwrap();
        assert!(!v.path.exists());
        assert!(vs.get("a").is_err());

        // duplicate name
        assert!(vs
            .create(Some("b".into()), None, BTreeMap::new(), false)
            .await
            .is_err());
        vs.delete_snapshot(&snap.id).await.unwrap();
        assert!(!snap.path.exists());
    }
}
