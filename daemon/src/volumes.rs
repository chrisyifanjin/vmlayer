//! The volume store: creates, clones and snapshots volume data on the host.
//!
//! v1 keeps every volume as a directory under `<data_dir>/volumes/<id>` and
//! every snapshot under `<data_dir>/volume-snapshots/<id>`. Clone and
//! snapshot are copy-on-write where the filesystem allows it (`cp
//! --reflink=auto` on Linux, `cp -c` on APFS) and plain copies otherwise.
//! Block-image volumes for microVM backends hold an `image.ext4` inside
//! that directory (ADR 0006 D11).
//!
//! With a blob store configured, a durable snapshot is also pushed to object
//! storage as content-addressed chunks (ADR 0011). Its local copy can then be
//! dropped, and any daemon sharing the store can create a volume from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::process::Command;
use uuid::Uuid;
use vmlayer_core::{Error, Result, VolumeId, VolumeSnapshotId, VolumeSource, VolumeSourceKind};

use vmlayer_blob::{Manifest, Remote};

use crate::store::{Store, StoreError, VolumeRow, VolumeSnapshotRow};

#[derive(Clone)]
pub struct VolumeStore {
    root: PathBuf,
    store: Store,
    default_size_mib: u64,
    /// Backends that can scrub directories the daemon cannot delete itself
    /// (containers write as root into bind-mounted volume directories).
    cleaners: std::sync::Arc<std::sync::Mutex<Vec<std::sync::Arc<dyn vmlayer_core::Backend>>>>,
    /// Object storage for durable snapshots, if configured.
    remote: Option<Remote>,
    /// Restore block-image snapshots on demand (decision 0016).
    lazy_restore: bool,
    /// Volumes still being downloaded.
    lazy: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<VolumeId, std::sync::Arc<vmlayer_blob::LazyImage>>,
        >,
    >,
    /// Lazy volumes handed to a sandbox's backend (its shim serves and
    /// downloads them; we keep our hands off until it lets go).
    handed: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<VolumeId>>>,
    /// The blob store as configured, for backends that read it themselves.
    remote_config: Option<(String, BTreeMap<String, String>)>,
}

/// Marks a volume that is still being downloaded; holds the snapshot id.
const LAZY_FILE: &str = "lazy.json";

fn lazy_snapshot(volume_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(volume_dir.join(LAZY_FILE)).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("snapshot")?
        .as_str()
        .map(String::from)
}

fn blob_err(e: vmlayer_blob::Error) -> Error {
    match e {
        vmlayer_blob::Error::NotFound(id) => {
            Error::Guest(format!("volume snapshot {id} not found"))
        }
        other => Error::Engine {
            backend: "blob-store",
            message: other.to_string(),
            source: Some(Box::new(other)),
        },
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
            cleaners: Default::default(),
            remote: None,
            lazy_restore: false,
            lazy: Default::default(),
            handed: Default::default(),
            remote_config: None,
        }
    }

    /// Restore block-image snapshots on demand where the host supports it.
    pub fn with_lazy_restore(mut self, on: bool) -> Self {
        self.lazy_restore = on;
        self
    }

    /// The blob store URL and options, passed to backends that serve lazy
    /// volumes themselves.
    pub fn with_remote_config(mut self, url: String, options: BTreeMap<String, String>) -> Self {
        self.remote_config = Some((url, options));
        self
    }

    /// Whether a lazy volume can be served to a VM on demand here (the NBD
    /// kernel driver); otherwise it is downloaded before it is attached.
    fn can_serve_lazily(&self) -> bool {
        cfg!(target_os = "linux") && Path::new("/dev/nbd0").exists() && self.remote_config.is_some()
    }

    /// Keep downloading `img` until it is complete or the volume is gone.
    fn spawn_hydrate(&self, id: VolumeId, img: std::sync::Arc<vmlayer_blob::LazyImage>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                if !this.lazy.lock().unwrap().contains_key(&id) {
                    return; // deleted
                }
                match img.hydrate(16).await {
                    Ok(()) => break,
                    // Handed to a sandbox; its shim downloads now.
                    Err(vmlayer_blob::Error::Closed) => return,
                    Err(e) => {
                        tracing::warn!(volume = %id, error = %e, "lazy download failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
            if let Ok(row) = this.get(&id.0) {
                let _ = std::fs::remove_file(row.path.join(LAZY_FILE));
            }
            this.forget_lazy_if_idle(&id);
        });
    }

    fn forget_lazy_if_idle(&self, id: &VolumeId) {
        let mut lazy = self.lazy.lock().unwrap();
        if lazy.get(id).is_some_and(|i| i.is_complete()) {
            lazy.remove(id);
        }
    }

    /// Resume downloads interrupted by a daemon restart.
    pub async fn resume_lazy(&self) {
        for row in self.list().unwrap_or_default() {
            if row.attached_to.is_some() {
                continue; // a running sandbox's shim owns it; resumed on release
            }
            if let Err(e) = self.lazy_image(&row).await {
                // The marker stays, so the volume keeps being refused (and
                // resumption is retried) instead of passing as complete.
                tracing::warn!(volume = %row.id, error = %e, "lazy volume cannot resume yet");
            }
        }
    }

    /// The download state of a volume restored lazily: `None` once it is
    /// complete (or was never lazy). A volume whose download was interrupted
    /// is resumed here; if that is impossible right now the volume is
    /// refused rather than used with missing blocks (review T0.2).
    async fn lazy_image(
        &self,
        row: &VolumeRow,
    ) -> Result<Option<std::sync::Arc<vmlayer_blob::LazyImage>>> {
        if let Some(img) = self.lazy.lock().unwrap().get(&row.id).cloned() {
            return Ok(Some(img));
        }
        let marker = row.path.join(LAZY_FILE);
        let Ok(text) = tokio::fs::read_to_string(&marker).await else {
            return Ok(None);
        };
        let incomplete = |why: String| {
            Error::Guest(format!(
                "volume {} is incomplete and its download cannot resume: {why}",
                row.id
            ))
        };
        let snapshot = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["snapshot"].as_str().map(String::from))
            .ok_or_else(|| incomplete(format!("unreadable marker {}", marker.display())))?;
        let remote = self
            .remote
            .clone()
            .ok_or_else(|| incomplete("no blob store is configured".into()))?;
        let manifest = remote
            .manifest(&snapshot)
            .await
            .map_err(|e| incomplete(e.to_string()))?;
        let img = vmlayer_blob::LazyImage::open(remote, &manifest, &row.path.join(IMAGE_FILE))
            .map_err(|e| incomplete(e.to_string()))?;
        let img = {
            let mut lazy = self.lazy.lock().unwrap();
            lazy.entry(row.id.clone()).or_insert(img).clone()
        };
        self.spawn_hydrate(row.id.clone(), img.clone());
        tracing::info!(volume = %row.id, "resumed lazy download");
        Ok(Some(img))
    }

    /// Wait until a lazily restored volume is fully local.
    pub async fn complete(&self, id: &VolumeId) -> Result<()> {
        let row = self.get(&id.0)?;
        if let Some(img) = self.lazy_image(&row).await? {
            img.hydrate(16).await.map_err(blob_err)?;
            self.forget_lazy_if_idle(id);
        }
        Ok(())
    }

    /// Get a volume ready to be attached. A lazy volume is handed to the
    /// sandbox's backend, which serves it on demand (decision 0019): our own
    /// download stops first, with every in-flight write finished, so exactly
    /// one process ever writes the image. Read-only attachments, and hosts
    /// that cannot serve lazily, wait for the download instead.
    pub async fn prepare(&self, row: &VolumeRow, read_only: bool) -> Result<()> {
        let Some(img) = self.lazy_image(row).await? else {
            return Ok(());
        };
        if img.is_complete() || read_only || !self.can_serve_lazily() {
            return self.complete(&row.id).await;
        }
        self.lazy.lock().unwrap().remove(&row.id);
        img.close().await;
        self.handed.lock().unwrap().insert(row.id.clone());
        Ok(())
    }

    /// Take lazy volumes back from sandboxes that are gone, and resume their
    /// downloads (also for volumes a previous daemon handed over).
    pub async fn release_unattached(&self) {
        let rows = self.list().unwrap_or_default();
        for row in rows.iter().filter(|r| r.attached_to.is_none()) {
            self.handed.lock().unwrap().remove(&row.id);
            if row.path.join(LAZY_FILE).exists() && !self.lazy.lock().unwrap().contains_key(&row.id)
            {
                if let Err(e) = self.lazy_image(row).await {
                    tracing::warn!(volume = %row.id, error = %e, "lazy volume cannot resume yet");
                }
            }
        }
    }

    /// Use `remote` for durable snapshots.
    pub fn with_remote(mut self, remote: Option<Remote>) -> Self {
        self.remote = remote;
        self
    }

    pub fn remote(&self) -> Option<&Remote> {
        self.remote.as_ref()
    }

    fn require_remote(&self) -> Result<&Remote> {
        self.remote.as_ref().ok_or_else(|| {
            Error::Guest(
                "no blob store is configured (set [blob_store] url or --blob-store)".into(),
            )
        })
    }

    pub fn set_cleaners(&self, backends: Vec<std::sync::Arc<dyn vmlayer_core::Backend>>) {
        *self.cleaners.lock().unwrap() = backends;
    }

    /// Delete a directory under the data dir; when permission is denied,
    /// let a backend scrub it first.
    async fn remove_tree(&self, path: &Path) {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "direct removal failed; asking backends")
            }
        }
        let cleaners = self.cleaners.lock().unwrap().clone();
        for be in cleaners {
            match be.cleanup_dir(path).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => tracing::warn!(backend = be.name(), error = %e, "cleanup failed"),
            }
        }
        if let Err(e) = tokio::fs::remove_dir_all(path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "could not remove directory");
            }
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
        if self.handed.lock().unwrap().contains(&row.id) {
            if let (Some((url, options)), Some(snapshot)) =
                (&self.remote_config, lazy_snapshot(&row.path))
            {
                return VolumeSource::LazyBlockImage {
                    path: row.path.join(IMAGE_FILE),
                    snapshot,
                    store_url: url.clone(),
                    store_options: options.clone(),
                };
            }
        }
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
        self.lazy.lock().unwrap().remove(id);
        self.store.delete_volume(id).map_err(store_err)?;
        if row.path.starts_with(&self.root) {
            self.remove_tree(&row.path).await;
        }
        Ok(())
    }

    /// Snapshot a volume. With `durable`, also push it to the blob store;
    /// if that fails, no snapshot is kept.
    pub async fn snapshot(
        &self,
        id: &VolumeId,
        guest_path: Option<String>,
        mut labels: BTreeMap<String, String>,
        durable: bool,
    ) -> Result<VolumeSnapshotRow> {
        self.complete(id).await?;
        let row = self.get(&id.0)?;
        crate::auth::inherit_tenant(&mut labels, &row.labels);
        let remote = if durable {
            Some(self.require_remote()?)
        } else {
            None
        };
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
            durable,
            labels,
            path,
        };
        if let Some(remote) = remote {
            if let Err(e) = self.push(remote, &snap, row.size_mib).await {
                self.remove_tree(&snap.path).await;
                return Err(e);
            }
        }
        self.store
            .insert_volume_snapshot(&snap)
            .map_err(store_err)?;
        Ok(snap)
    }

    async fn push(&self, remote: &Remote, snap: &VolumeSnapshotRow, size_mib: u64) -> Result<()> {
        let meta = vmlayer_blob::SnapshotMeta {
            id: snap.id.0.clone(),
            volume_id: snap.volume_id.0.clone(),
            size_mib,
            guest_path: snap.guest_path.clone(),
            labels: snap.labels.clone(),
            created_at: unix_secs(snap.created_at),
        };
        let image = snap.path.join(IMAGE_FILE);
        for attempt in 0..3 {
            let pushed = if image.is_file() {
                remote.push_block(&image, &meta).await
            } else {
                remote.push_dir(&snap.path, &meta).await
            };
            match pushed {
                Ok(_) => return Ok(()),
                // A concurrent garbage collection took a chunk we reused.
                Err(vmlayer_blob::Error::Raced(chunk)) if attempt < 2 => {
                    tracing::info!(snapshot = %snap.id, %chunk, "upload raced a garbage collection; retrying");
                }
                Err(e) => return Err(blob_err(e)),
            }
        }
        unreachable!("the last attempt returns")
    }

    /// Snapshots in the blob store, from any daemon that shares it.
    pub async fn list_remote(&self) -> Result<Vec<Manifest>> {
        self.require_remote()?.list().await.map_err(blob_err)
    }

    /// Delete unreferenced chunks from the blob store.
    pub async fn collect_garbage(&self, dry_run: bool) -> Result<vmlayer_blob::GcStats> {
        self.require_remote()?.gc(dry_run).await.map_err(blob_err)
    }

    pub fn get_snapshot(&self, id: &VolumeSnapshotId) -> Result<VolumeSnapshotRow> {
        self.store.get_volume_snapshot(id).map_err(|e| match e {
            StoreError::NotFound => Error::Guest(format!("volume snapshot {id} not found")),
            e => store_err(e),
        })
    }

    /// Labels of a volume snapshot, here or in the blob store; `None` if
    /// neither has it.
    pub async fn snapshot_labels(&self, id: &VolumeSnapshotId) -> Option<BTreeMap<String, String>> {
        if let Ok(s) = self.get_snapshot(id) {
            return Some(s.labels);
        }
        let remote = self.remote.as_ref()?;
        remote.manifest(&id.0).await.ok().map(|m| m.labels)
    }

    pub fn list_snapshots(&self) -> Result<Vec<VolumeSnapshotRow>> {
        self.store.list_volume_snapshots().map_err(store_err)
    }

    /// Delete a snapshot, here and in the blob store. With `local_only`,
    /// drop just the local copy of a durable snapshot and keep it in the
    /// blob store, where `from_snapshot` will fetch it.
    pub async fn delete_snapshot(&self, id: &VolumeSnapshotId, local_only: bool) -> Result<()> {
        let row = match self.get_snapshot(id) {
            Ok(row) => row,
            // Known only to the blob store (pushed by another daemon).
            Err(_) if !local_only && self.remote.is_some() => {
                let remote = self.require_remote()?;
                remote.manifest(&id.0).await.map_err(blob_err)?;
                return remote.delete(&id.0).await.map_err(blob_err);
            }
            Err(e) => return Err(e),
        };
        if local_only {
            if !row.durable {
                return Err(Error::Guest(format!(
                    "volume snapshot {id} is not durable; its local copy is the only one"
                )));
            }
        } else {
            if row.durable {
                self.require_remote()?
                    .delete(&id.0)
                    .await
                    .map_err(blob_err)?;
            }
            self.store.delete_volume_snapshot(id).map_err(store_err)?;
        }
        if row.path.starts_with(&self.root) {
            self.remove_tree(&row.path).await;
        }
        Ok(())
    }

    /// A new volume whose contents are a clone of a snapshot.
    pub async fn from_snapshot(
        &self,
        snapshot: &VolumeSnapshotId,
        name: Option<String>,
        mut labels: BTreeMap<String, String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        let local = self.get_snapshot(snapshot);
        let snap = match local {
            Ok(snap) if snap.path.is_dir() => snap,
            Ok(snap) if !snap.durable => {
                return Err(Error::Guest(format!(
                    "the data of volume snapshot {snapshot} is missing from {}",
                    snap.path.display()
                )))
            }
            // Durable with the local copy dropped, or pushed by another daemon.
            Ok(_) | Err(_) if self.remote.is_some() => {
                return self.pull(snapshot, name, labels, anonymous).await
            }
            Err(e) => return Err(e),
            Ok(_) => unreachable!("guarded above"),
        };
        crate::auth::inherit_tenant(&mut labels, &snap.labels);
        let src_vol = self.get(&snap.volume_id.0).ok();
        let row = self
            .create(name, src_vol.map(|v| v.size_mib), labels, anonymous)
            .await?;
        if let Err(e) = clone_tree(&snap.path, &row.path).await {
            let _ = self.delete(&row.id).await;
            return Err(e);
        }
        Ok(row)
    }

    /// A new volume holding snapshot `snapshot`, downloaded from the blob store.
    async fn pull(
        &self,
        snapshot: &VolumeSnapshotId,
        name: Option<String>,
        mut labels: BTreeMap<String, String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        let remote = self.require_remote()?;
        let manifest = remote.manifest(&snapshot.0).await.map_err(blob_err)?;
        crate::auth::inherit_tenant(&mut labels, &manifest.labels);
        // An empty directory; the snapshot brings the image for block volumes.
        let row = self
            .create(name, Some(manifest.size_mib), labels, anonymous)
            .await?;
        let dest = match manifest.kind {
            vmlayer_blob::Kind::Block => row.path.join(IMAGE_FILE),
            vmlayer_blob::Kind::Dir => row.path.clone(),
        };
        if manifest.kind == vmlayer_blob::Kind::Block && self.lazy_restore {
            // Usable at once; chunks arrive on first use or in the background.
            let img = match vmlayer_blob::LazyImage::open(remote.clone(), &manifest, &dest) {
                Ok(img) => img,
                Err(e) => {
                    let _ = self.delete(&row.id).await;
                    return Err(blob_err(e));
                }
            };
            let marker = serde_json::json!({ "snapshot": snapshot.0 }).to_string();
            tokio::fs::write(row.path.join(LAZY_FILE), marker).await?;
            self.lazy
                .lock()
                .unwrap()
                .insert(row.id.clone(), img.clone());
            self.spawn_hydrate(row.id.clone(), img);
            tracing::info!(snapshot = %snapshot, volume = %row.id, "volume restoring lazily");
            return Ok(row);
        }
        let started = std::time::Instant::now();
        if let Err(e) = remote.pull(&manifest, &dest).await {
            let _ = self.delete(&row.id).await;
            return Err(blob_err(e));
        }
        tracing::info!(
            snapshot = %snapshot,
            volume = %row.id,
            bytes = manifest.stored_bytes(),
            ms = started.elapsed().as_millis() as u64,
            "volume restored from the blob store"
        );
        Ok(row)
    }

    /// A new volume whose contents are a clone of another volume's current state.
    pub async fn clone_volume(
        &self,
        id: &VolumeId,
        name: Option<String>,
        anonymous: bool,
    ) -> Result<VolumeRow> {
        self.complete(id).await?;
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
            .snapshot(&v.id, Some("/work".into()), BTreeMap::new(), false)
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
        vs.delete_snapshot(&snap.id, false).await.unwrap();
        assert!(!snap.path.exists());
    }

    #[tokio::test]
    async fn interrupted_lazy_volumes_are_refused_until_they_resume() {
        let blobs = tempfile::tempdir().unwrap();
        let url = format!("file://{}", blobs.path().display());
        let remote = Remote::open(&url, &BTreeMap::new()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let vs = VolumeStore::new(dir.path(), Store::open_in_memory().unwrap(), 64)
            .with_remote(Some(remote.clone()))
            .with_lazy_restore(true);
        // A block volume whose download was interrupted by a restart, with
        // its snapshot manifest unreachable.
        let v = vs
            .create(None, Some(8), BTreeMap::new(), false)
            .await
            .unwrap();
        let src = dir.path().join("src.ext4");
        std::fs::File::create(&src)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        std::fs::write(&src, b"real data").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        let meta = vmlayer_blob::SnapshotMeta {
            id: "vsnap-gone".into(),
            size_mib: 8,
            ..Default::default()
        };
        let (manifest, _) = remote.push_block(&src, &meta).await.unwrap();
        let image = v.path.join(IMAGE_FILE);
        std::fs::File::create(&image)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        std::fs::write(v.path.join(LAZY_FILE), r#"{"snapshot":"vsnap-gone"}"#).unwrap();
        remote.delete("vsnap-gone").await.unwrap();

        vs.resume_lazy().await; // logs, does not pretend
        let err = vs.prepare(&v, false).await.unwrap_err();
        assert!(err.to_string().contains("incomplete"), "{err}");
        assert!(vs
            .snapshot(&v.id, None, BTreeMap::new(), false)
            .await
            .is_err());

        // Once the manifest is reachable again the volume resumes and completes.
        let (_, _) = remote
            .push_block(
                &src,
                &vmlayer_blob::SnapshotMeta {
                    id: manifest.id.clone(),
                    size_mib: 8,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        vs.complete(&v.id).await.unwrap();
        assert_eq!(&std::fs::read(&image).unwrap()[..9], b"real data");
    }

    #[tokio::test]
    async fn durable_snapshots_move_between_daemons() {
        let blobs = tempfile::tempdir().unwrap();
        let url = format!("file://{}", blobs.path().display());
        let open = || Remote::open(&url, &BTreeMap::new()).unwrap();
        let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = VolumeStore::new(dir_a.path(), Store::open_in_memory().unwrap(), 100)
            .with_remote(Some(open()));
        let b = VolumeStore::new(dir_b.path(), Store::open_in_memory().unwrap(), 100)
            .with_remote(Some(open()));

        let v = a
            .create(None, Some(64), BTreeMap::new(), false)
            .await
            .unwrap();
        tokio::fs::create_dir_all(v.path.join("src")).await.unwrap();
        tokio::fs::write(v.path.join("src/app.py"), b"print(1)\n")
            .await
            .unwrap();
        let snap = a
            .snapshot(&v.id, None, BTreeMap::new(), true)
            .await
            .unwrap();
        assert!(snap.durable);

        // Another daemon sees it and restores it.
        let listed = b.list_remote().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].id.as_str(), listed[0].size_mib),
            (snap.id.0.as_str(), 64)
        );
        let restored = b
            .from_snapshot(&snap.id, Some("copy".into()), BTreeMap::new(), false)
            .await
            .unwrap();
        assert_eq!(restored.size_mib, 64);
        assert_eq!(
            tokio::fs::read(restored.path.join("src/app.py"))
                .await
                .unwrap(),
            b"print(1)\n"
        );

        // Dropping the local copy keeps it restorable on the first daemon.
        a.delete_snapshot(&snap.id, true).await.unwrap();
        assert!(!snap.path.exists());
        let again = a
            .from_snapshot(&snap.id, None, BTreeMap::new(), true)
            .await
            .unwrap();
        assert!(again.path.join("src/app.py").is_file());

        // A full delete removes it from the blob store for everyone.
        a.delete_snapshot(&snap.id, false).await.unwrap();
        assert!(b.list_remote().await.unwrap().is_empty());
        assert!(b
            .from_snapshot(&snap.id, None, BTreeMap::new(), true)
            .await
            .is_err());

        // Without a blob store, durable snapshots are refused up front.
        let plain = VolumeStore::new(dir_b.path(), Store::open_in_memory().unwrap(), 100);
        let w = plain
            .create(None, None, BTreeMap::new(), false)
            .await
            .unwrap();
        let err = plain
            .snapshot(&w.id, None, BTreeMap::new(), true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no blob store"), "{err}");
    }
}
