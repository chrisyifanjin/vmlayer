//! The sandbox manager: everything between the gRPC surface and the backend
//! trait. Owns the live handle table, the process tables, TTLs, volume
//! attachment, snapshot composition and restore.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex, RwLock};
use vmlayer_core::{
    Capabilities, Endpoint, Error, ExecRequest, Mount, NetworkMode, Process, Pty, Result,
    SandboxHandle, SandboxId, SandboxSpec, SandboxState, SnapshotId, SnapshotOptions, SnapshotRef,
    VolumeId, VolumeSnapshotRef,
};

use crate::config::Config;
use crate::registry::Registry;
use crate::store::{MountRow, SandboxRow, Store, StoreError};
use crate::volumes::{new_id, VolumeStore};
use vmlayer_procs::{Entry, ProcessTable};

pub struct Live {
    pub id: SandboxId,
    pub backend: String,
    pub handle: Box<dyn SandboxHandle>,
    pub procs: ProcessTable,
    pub exposed: Mutex<BTreeMap<u16, Endpoint>>,
    ttl_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone, Debug)]
pub enum WorkspaceSpec {
    Anonymous,
    Volume(String),
    None,
}

#[derive(Clone, Debug)]
pub enum EntrypointSpec {
    ImageDefault,
    Argv(Vec<String>),
    None,
}

#[derive(Clone, Debug)]
pub struct MountParam {
    pub volume: String,
    pub guest_path: String,
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct CreateParams {
    pub image: String,
    pub vcpus: Option<u32>,
    pub memory_mib: Option<u32>,
    pub disk_mib: Option<u32>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub workspace: WorkspaceSpec,
    pub mounts: Vec<MountParam>,
    pub entrypoint: EntrypointSpec,
    pub ttl: Option<Duration>,
    pub backend: String,
    pub network: Option<NetworkMode>,
    pub labels: BTreeMap<String, String>,
}

/// What callers see: the stored row plus live state.
#[derive(Clone, Debug)]
pub struct SandboxView {
    pub row: SandboxRow,
    pub state: SandboxState,
    pub capabilities: Capabilities,
    pub mounts: Vec<MountRow>,
    pub exposed: BTreeMap<u16, Endpoint>,
}

pub struct Daemon {
    pub cfg: Config,
    pub registry: Registry,
    pub store: Store,
    pub volumes: VolumeStore,
    live: RwLock<HashMap<SandboxId, Arc<Live>>>,
}

pub(crate) fn store_err(e: StoreError) -> Error {
    match e {
        StoreError::NotFound => Error::Guest("not found".into()),
        other => Error::Engine {
            backend: "daemon",
            message: other.to_string(),
            source: Some(Box::new(other)),
        },
    }
}

impl Daemon {
    pub async fn new(cfg: Config) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let store = Store::open(&cfg.data_dir.join("state.db"))?;
        let volumes = VolumeStore::new(&cfg.data_dir, store.clone(), cfg.defaults.volume_size_mib);
        let registry = Registry::from_config(&cfg).await;
        let d = Arc::new(Self {
            cfg,
            registry,
            store,
            volumes,
            live: RwLock::new(HashMap::new()),
        });
        d.reattach().await;
        Ok(d)
    }

    /// After a daemon restart, pick up sandboxes the backends still run.
    async fn reattach(self: &Arc<Self>) {
        let rows = match self.store.list_sandboxes() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "listing sandboxes at startup");
                return;
            }
        };
        for row in rows {
            let Some(entry) = self.registry.get(&row.backend) else {
                tracing::warn!(id = %row.id, backend = %row.backend, "backend gone; marking failed");
                let _ = self.store.set_sandbox_state(&row.id, SandboxState::Failed);
                continue;
            };
            match entry.backend.attach(&row.id).await {
                Ok(handle) => {
                    let live = self
                        .register(row.id.clone(), row.backend.clone(), handle)
                        .await;
                    self.arm_ttl(&live, row.created_at, row.ttl);
                    tracing::info!(id = %row.id, "reattached");
                }
                Err(e) => {
                    tracing::warn!(id = %row.id, error = %e, "could not reattach; marking failed");
                    let _ = self.store.set_sandbox_state(&row.id, SandboxState::Failed);
                    let _ = self.volumes.detach_all(&row.id);
                }
            }
        }
    }

    async fn register(
        &self,
        id: SandboxId,
        backend: String,
        handle: Box<dyn SandboxHandle>,
    ) -> Arc<Live> {
        let live = Arc::new(Live {
            id: id.clone(),
            backend,
            handle,
            procs: ProcessTable::new(self.cfg.defaults.output_buffer_bytes),
            exposed: Mutex::new(BTreeMap::new()),
            ttl_task: Mutex::new(None),
        });
        self.live.write().await.insert(id, live.clone());
        live
    }

    fn arm_ttl(self: &Arc<Self>, live: &Arc<Live>, created_at: SystemTime, ttl: Option<Duration>) {
        let Some(ttl) = ttl else { return };
        let elapsed = SystemTime::now()
            .duration_since(created_at)
            .unwrap_or_default();
        let remaining = ttl.saturating_sub(elapsed);
        let d = self.clone();
        let id = live.id.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(remaining).await;
            tracing::info!(%id, "ttl elapsed; killing");
            if let Err(e) = d.kill(&id).await {
                tracing::warn!(%id, error = %e, "ttl kill failed");
            }
        });
        let live = live.clone();
        tokio::spawn(async move {
            *live.ttl_task.lock().await = Some(task);
        });
    }

    pub async fn live(&self, id: &SandboxId) -> Result<Arc<Live>> {
        self.live
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| Error::SandboxNotFound(id.clone()))
    }

    // -- create / restore ---------------------------------------------------

    async fn resolve_mounts(
        &self,
        id: &SandboxId,
        workspace: &WorkspaceSpec,
        workdir: &str,
        extra: &[MountParam],
    ) -> Result<(Option<VolumeId>, Vec<Mount>, Vec<MountRow>)> {
        let mut mounts = Vec::new();
        let mut rows = Vec::new();
        let mut attached: Vec<VolumeId> = Vec::new();
        let mut push = |vol: &crate::store::VolumeRow, guest_path: &str, read_only: bool| {
            mounts.push(Mount {
                volume: vol.id.clone(),
                source: self.volumes.source(vol),
                guest_path: guest_path.to_string(),
                read_only,
            });
            rows.push(MountRow {
                sandbox_id: id.clone(),
                volume_id: vol.id.clone(),
                guest_path: guest_path.to_string(),
                read_only,
            });
        };
        let workspace_id = match workspace {
            WorkspaceSpec::Anonymous => {
                let v = self
                    .volumes
                    .create(None, None, BTreeMap::new(), true)
                    .await?;
                self.volumes.attach(&v.id, id, false)?;
                attached.push(v.id.clone());
                push(&v, workdir, false);
                Some(v.id)
            }
            WorkspaceSpec::Volume(name) => {
                let v = self.volumes.get(name)?;
                if let Err(e) = self.volumes.attach(&v.id, id, false) {
                    self.rollback_attach(id, &attached).await;
                    return Err(e);
                }
                attached.push(v.id.clone());
                push(&v, workdir, false);
                Some(v.id)
            }
            WorkspaceSpec::None => None,
        };
        for m in extra {
            let v = match self.volumes.get(&m.volume) {
                Ok(v) => v,
                Err(e) => {
                    self.rollback_attach(id, &attached).await;
                    return Err(e);
                }
            };
            if let Err(e) = self.volumes.attach(&v.id, id, m.read_only) {
                self.rollback_attach(id, &attached).await;
                return Err(e);
            }
            if !m.read_only {
                attached.push(v.id.clone());
            }
            push(&v, &m.guest_path, m.read_only);
        }
        Ok((workspace_id, mounts, rows))
    }

    async fn rollback_attach(&self, id: &SandboxId, _attached: &[VolumeId]) {
        let _ = self.volumes.detach_all(id);
        for v in self.volumes.list().unwrap_or_default() {
            if v.anonymous && v.attached_to.is_none() && !self.volume_in_use(&v.id) {
                let _ = self.volumes.delete(&v.id).await;
            }
        }
    }

    fn volume_in_use(&self, vol: &VolumeId) -> bool {
        self.store
            .list_sandboxes()
            .unwrap_or_default()
            .iter()
            .any(|s| s.workspace_volume_id.as_ref() == Some(vol))
    }

    pub async fn create(self: &Arc<Self>, p: CreateParams) -> Result<SandboxView> {
        let (backend_name, entry) = self.registry.pick(&p.backend)?;
        let resolved = entry.resolver.resolve(&p.image).await?;
        let id = SandboxId(new_id("sb"));
        // The image's WORKDIR is only a useful workspace mount point when it
        // is a real directory; `/` or unset falls back to the daemon default.
        let workdir = p
            .workdir
            .clone()
            .filter(|w| !w.is_empty())
            .or(resolved
                .workdir
                .clone()
                .filter(|w| w != "/" && !w.is_empty()))
            .unwrap_or_else(|| self.cfg.defaults.workdir.clone());
        let (workspace_id, mounts, mount_rows) = self
            .resolve_mounts(&id, &p.workspace, &workdir, &p.mounts)
            .await?;
        let mut env = resolved.env.clone();
        env.extend(p.env.clone());
        let entrypoint = match &p.entrypoint {
            EntrypointSpec::ImageDefault => resolved.entrypoint.clone(),
            EntrypointSpec::Argv(a) => Some(a.clone()),
            EntrypointSpec::None => None,
        }
        .filter(|a| !a.is_empty())
        .map(|argv| ExecRequest {
            argv,
            cwd: Some(workdir.clone()),
            env: BTreeMap::new(),
            timeout: None,
            pty: None,
        });
        let spec = SandboxSpec {
            id: id.clone(),
            rootfs: resolved.rootfs.clone(),
            mounts,
            vcpus: p.vcpus.unwrap_or(self.cfg.defaults.vcpus).max(1),
            memory_mib: p.memory_mib.unwrap_or(self.cfg.defaults.memory_mib).max(64),
            disk_mib: p.disk_mib,
            env,
            workdir: workdir.clone(),
            entrypoint,
            network: p.network.unwrap_or(NetworkMode::EgressAndExpose),
            ttl: p.ttl,
            labels: p.labels.clone(),
        };
        let handle = match entry.backend.create(spec.clone()).await {
            Ok(h) => h,
            Err(e) => {
                self.rollback_attach(&id, &[]).await;
                return Err(e);
            }
        };
        let row = SandboxRow {
            id: id.clone(),
            backend: backend_name.clone(),
            image: p.image.clone(),
            workdir,
            workspace_volume_id: workspace_id,
            state: SandboxState::Running,
            created_at: SystemTime::now(),
            ttl: p.ttl,
            labels: p.labels,
            spec,
        };
        self.store.insert_sandbox(&row).map_err(store_err)?;
        for m in &mount_rows {
            self.store.insert_mount(m).map_err(store_err)?;
        }
        let live = self.register(id.clone(), backend_name, handle).await;
        self.arm_ttl(&live, row.created_at, row.ttl);
        tracing::info!(%id, image = %row.image, backend = %row.backend, "created");
        self.view(&id).await
    }

    pub async fn restore(
        self: &Arc<Self>,
        snapshot_id: &SnapshotId,
        ttl: Option<Duration>,
        labels: BTreeMap<String, String>,
    ) -> Result<SandboxView> {
        let snap = self.store.get_snapshot(snapshot_id).map_err(|e| match e {
            StoreError::NotFound => Error::SnapshotNotFound(snapshot_id.clone()),
            e => store_err(e),
        })?;
        let entry =
            self.registry
                .get(&snap.backend)
                .ok_or_else(|| Error::SnapshotIncompatible {
                    id: snap.id.clone(),
                    reason: format!("backend {} not available here", snap.backend),
                })?;
        let id = SandboxId(new_id("sb"));
        // Fresh copy-on-write clones of every volume (ADR 0006 D5).
        let mut mounts = Vec::new();
        let mut rows = Vec::new();
        let mut workspace_id = None;
        for vs in &snap.volumes {
            let vol = self
                .volumes
                .from_snapshot(&vs.id, None, BTreeMap::new(), true)
                .await?;
            let guest_path = vs.guest_path.clone().unwrap_or_default();
            let read_only = snap
                .spec
                .mounts
                .iter()
                .find(|m| m.guest_path == guest_path)
                .map(|m| m.read_only)
                .unwrap_or(false);
            self.volumes.attach(&vol.id, &id, read_only)?;
            if guest_path == snap.spec.workdir {
                workspace_id = Some(vol.id.clone());
            }
            mounts.push(Mount {
                volume: vol.id.clone(),
                source: self.volumes.source(&vol),
                guest_path: guest_path.clone(),
                read_only,
            });
            rows.push(MountRow {
                sandbox_id: id.clone(),
                volume_id: vol.id,
                guest_path,
                read_only,
            });
        }
        let opts = vmlayer_core::RestoreOptions {
            id: Some(id.clone()),
            ttl,
            mounts: mounts.clone(),
            labels: labels.clone(),
        };
        let handle = match entry.backend.restore(&snap, opts).await {
            Ok(h) => h,
            Err(e) => {
                self.rollback_attach(&id, &[]).await;
                return Err(e);
            }
        };
        let mut spec = snap.spec.clone();
        spec.id = id.clone();
        spec.mounts = mounts;
        spec.ttl = ttl.or(spec.ttl);
        spec.labels.extend(labels.clone());
        let row = SandboxRow {
            id: id.clone(),
            backend: snap.backend.clone(),
            image: format!("snapshot:{}", snap.id),
            workdir: spec.workdir.clone(),
            workspace_volume_id: workspace_id,
            state: SandboxState::Running,
            created_at: SystemTime::now(),
            ttl: spec.ttl,
            labels: spec.labels.clone(),
            spec,
        };
        self.store.insert_sandbox(&row).map_err(store_err)?;
        for m in &rows {
            self.store.insert_mount(m).map_err(store_err)?;
        }
        let live = self
            .register(id.clone(), snap.backend.clone(), handle)
            .await;
        self.arm_ttl(&live, row.created_at, row.ttl);
        tracing::info!(%id, snapshot = %snap.id, "restored");
        self.view(&id).await
    }

    // -- read -------------------------------------------------------------

    pub async fn view(&self, id: &SandboxId) -> Result<SandboxView> {
        let row = self.store.get_sandbox(id).map_err(|e| match e {
            StoreError::NotFound => Error::SandboxNotFound(id.clone()),
            e => store_err(e),
        })?;
        let mounts = self.store.mounts_for(id).map_err(store_err)?;
        let capabilities = self
            .registry
            .capabilities_of(&row.backend)
            .unwrap_or(Capabilities::CONTAINER);
        let (state, exposed) = match self.live(id).await {
            Ok(live) => (
                live.handle.state().await.unwrap_or(row.state),
                live.exposed.lock().await.clone(),
            ),
            Err(_) => (row.state, BTreeMap::new()),
        };
        Ok(SandboxView {
            row,
            state,
            capabilities,
            mounts,
            exposed,
        })
    }

    pub async fn list(&self, labels: &BTreeMap<String, String>) -> Result<Vec<SandboxView>> {
        let mut out = Vec::new();
        for row in self.store.list_sandboxes().map_err(store_err)? {
            if labels.iter().all(|(k, v)| row.labels.get(k) == Some(v)) {
                if let Ok(v) = self.view(&row.id).await {
                    out.push(v);
                }
            }
        }
        Ok(out)
    }

    // -- lifecycle ----------------------------------------------------------

    pub async fn kill(&self, id: &SandboxId) -> Result<()> {
        let live = self.live.write().await.remove(id);
        if let Some(live) = &live {
            if let Some(t) = live.ttl_task.lock().await.take() {
                // The TTL task itself calls kill; aborting it from inside
                // would cancel this very call half-way through.
                if tokio::task::try_id() != Some(t.id()) {
                    t.abort();
                }
            }
            live.handle.kill().await?;
        } else if let Some(entry) = self
            .store
            .get_sandbox(id)
            .ok()
            .and_then(|r| self.registry.get(&r.backend))
        {
            // Not live (failed reattach) but the backend may still have it.
            if let Ok(h) = entry.backend.attach(id).await {
                let _ = h.kill().await;
            }
        } else {
            self.store
                .get_sandbox(id)
                .map_err(|_| Error::SandboxNotFound(id.clone()))?;
        }
        let row = self.store.get_sandbox(id).ok();
        self.volumes.detach_all(id)?;
        self.store.delete_sandbox(id).map_err(store_err)?;
        if let Some(row) = row {
            for v in self.volumes.list().unwrap_or_default() {
                let mine = Some(&v.id) == row.workspace_volume_id.as_ref()
                    || row.spec.mounts.iter().any(|m| m.volume == v.id);
                if mine && v.anonymous {
                    let _ = self.volumes.delete(&v.id).await;
                }
            }
        }
        tracing::info!(%id, "killed");
        Ok(())
    }

    pub async fn pause(&self, id: &SandboxId) -> Result<()> {
        let live = self.live(id).await?;
        live.handle.pause().await?;
        self.store
            .set_sandbox_state(id, SandboxState::Paused)
            .map_err(store_err)
    }

    pub async fn resume(&self, id: &SandboxId) -> Result<()> {
        let live = self.live(id).await?;
        live.handle.resume().await?;
        self.store
            .set_sandbox_state(id, SandboxState::Running)
            .map_err(store_err)
    }

    pub async fn snapshot(
        &self,
        id: &SandboxId,
        labels: BTreeMap<String, String>,
        stay_paused: bool,
    ) -> Result<SnapshotRef> {
        let live = self.live(id).await?;
        let row = self.store.get_sandbox(id).map_err(store_err)?;
        let engine_version = self
            .registry
            .get(&row.backend)
            .map(|e| e.engine_version.clone())
            .unwrap_or_default();
        // Backend snapshot first, leaving the sandbox paused so the volume
        // snapshots are consistent with it; then resume if asked.
        let mut snap = live
            .handle
            .snapshot(SnapshotOptions {
                labels: labels.clone(),
                stay_paused: true,
            })
            .await?;
        let mut volume_refs = Vec::new();
        let mut first_err = None;
        for m in self.store.mounts_for(id).map_err(store_err)? {
            match self
                .volumes
                .snapshot(&m.volume_id, Some(m.guest_path.clone()), labels.clone())
                .await
            {
                Ok(vs) => volume_refs.push(VolumeSnapshotRef {
                    id: vs.id,
                    volume: vs.volume_id,
                    guest_path: vs.guest_path,
                    created_at: vs.created_at,
                    size_bytes: vs.size_bytes,
                    durable: vs.durable,
                    labels: vs.labels,
                }),
                Err(e) => {
                    first_err = Some(e);
                    break;
                }
            }
        }
        if !stay_paused {
            let _ = live.handle.resume().await;
        } else {
            let _ = self.store.set_sandbox_state(id, SandboxState::Paused);
        }
        if let Some(e) = first_err {
            let _ = live.handle.backend();
            return Err(e);
        }
        snap.engine_version = engine_version;
        snap.volumes = volume_refs;
        self.store.insert_snapshot(&snap).map_err(store_err)?;
        tracing::info!(%id, snapshot = %snap.id, "snapshot taken");
        Ok(snap)
    }

    pub async fn delete_snapshot(&self, id: &SnapshotId) -> Result<()> {
        let snap = self.store.get_snapshot(id).map_err(|e| match e {
            StoreError::NotFound => Error::SnapshotNotFound(id.clone()),
            e => store_err(e),
        })?;
        if let Some(entry) = self.registry.get(&snap.backend) {
            entry.backend.delete_snapshot(&snap).await?;
        }
        for vs in &snap.volumes {
            let _ = self.volumes.delete_snapshot(&vs.id).await;
        }
        self.store.delete_snapshot(id).map_err(store_err)
    }

    pub fn list_snapshots(&self, labels: &BTreeMap<String, String>) -> Result<Vec<SnapshotRef>> {
        Ok(self
            .store
            .list_snapshots()
            .map_err(store_err)?
            .into_iter()
            .filter(|s| labels.iter().all(|(k, v)| s.labels.get(k) == Some(v)))
            .collect())
    }

    pub async fn fork(self: &Arc<Self>, id: &SandboxId, count: u32) -> Result<Vec<SandboxView>> {
        let live = self.live(id).await?;
        let row = self.store.get_sandbox(id).map_err(store_err)?;
        let caps = self
            .registry
            .capabilities_of(&row.backend)
            .unwrap_or(Capabilities::CONTAINER);
        if !caps.fork {
            return Err(Error::Unsupported {
                backend: self
                    .registry
                    .get(&row.backend)
                    .map(|_| "backend")
                    .unwrap_or("daemon"),
                feature: "fork",
            });
        }
        // Clone volumes per child, then let the backend fork.
        let mut per_child = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..count {
            let cid = SandboxId(new_id("sb"));
            let mut mounts = Vec::new();
            for m in self.store.mounts_for(id).map_err(store_err)? {
                let v = self.volumes.clone_volume(&m.volume_id, None, true).await?;
                self.volumes.attach(&v.id, &cid, m.read_only)?;
                let source = self.volumes.source(&v);
                mounts.push(Mount {
                    volume: v.id,
                    source,
                    guest_path: m.guest_path,
                    read_only: m.read_only,
                });
            }
            per_child.push(mounts);
            ids.push(cid);
        }
        let handles = live.handle.fork(count, per_child.clone()).await?;
        let mut views = Vec::new();
        for ((handle, cid), mounts) in handles.into_iter().zip(ids).zip(per_child) {
            let mut spec = row.spec.clone();
            spec.id = handle.id().clone();
            spec.mounts = mounts.clone();
            let child_row = SandboxRow {
                id: spec.id.clone(),
                backend: row.backend.clone(),
                image: row.image.clone(),
                workdir: row.workdir.clone(),
                workspace_volume_id: mounts
                    .iter()
                    .find(|m| m.guest_path == row.workdir)
                    .map(|m| m.volume.clone()),
                state: SandboxState::Running,
                created_at: SystemTime::now(),
                ttl: row.ttl,
                labels: row.labels.clone(),
                spec,
            };
            let _ = cid; // backend assigns the final id; volumes were attached under ours
            self.volumes.detach_all(&cid)?;
            for m in &mounts {
                self.volumes.attach(&m.volume, &child_row.id, m.read_only)?;
                self.store
                    .insert_mount(&MountRow {
                        sandbox_id: child_row.id.clone(),
                        volume_id: m.volume.clone(),
                        guest_path: m.guest_path.clone(),
                        read_only: m.read_only,
                    })
                    .map_err(store_err)?;
            }
            self.store.insert_sandbox(&child_row).map_err(store_err)?;
            let live = self
                .register(child_row.id.clone(), row.backend.clone(), handle)
                .await;
            self.arm_ttl(&live, child_row.created_at, child_row.ttl);
            views.push(self.view(&child_row.id).await?);
        }
        Ok(views)
    }

    // -- network -------------------------------------------------------------

    pub async fn expose(&self, id: &SandboxId, port: u16) -> Result<Endpoint> {
        let live = self.live(id).await?;
        let ep = live.handle.expose(port).await?;
        live.exposed.lock().await.insert(port, ep.clone());
        Ok(ep)
    }

    pub async fn unexpose(&self, id: &SandboxId, port: u16) -> Result<()> {
        let live = self.live(id).await?;
        live.handle.unexpose(port).await?;
        live.exposed.lock().await.remove(&port);
        Ok(())
    }

    // -- processes -----------------------------------------------------------

    pub async fn exec(&self, id: &SandboxId, req: ExecRequest) -> Result<(Arc<Live>, Arc<Entry>)> {
        let live = self.live(id).await?;
        let timeout = req.timeout;
        let argv = req.argv.clone();
        let process: Box<dyn Process> = live.handle.exec(req).await?;
        let entry = live.procs.adopt(process, argv, false, timeout).await;
        live.procs.gc(64).await;
        Ok((live, entry))
    }

    pub async fn pty(&self, id: &SandboxId, req: ExecRequest) -> Result<Box<dyn Pty>> {
        let live = self.live(id).await?;
        live.handle.pty(req).await
    }

    /// Find a process by pid: the daemon's table first, then the backend
    /// (for the entrypoint, which the backend starts itself).
    pub async fn process(&self, id: &SandboxId, pid: u32) -> Result<(Arc<Live>, Arc<Entry>)> {
        let live = self.live(id).await?;
        if let Some(e) = live.procs.get(pid).await {
            return Ok((live, e));
        }
        let process = live.handle.process(pid).await?;
        let info = live
            .handle
            .processes()
            .await?
            .into_iter()
            .find(|p| p.pid == pid);
        let argv = info.as_ref().map(|p| p.argv.clone()).unwrap_or_default();
        let is_entry = info.map(|p| p.is_entrypoint).unwrap_or(pid == 1);
        let entry = live.procs.adopt(process, argv, is_entry, None).await;
        Ok((live, entry))
    }
}
