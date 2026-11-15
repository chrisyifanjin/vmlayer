//! One sandbox = one jailed Firecracker VM plus a gRPC client to its guest agent.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;
use vmlayer_core::{
    Endpoint, Error, ExecRequest, FileInfo, Mount, Process, ProcessInfo, Pty, Result,
    SandboxHandle, SandboxId, SandboxInfo, SandboxSpec, SandboxState, SnapshotId, SnapshotOptions,
    SnapshotRef, VolumeSource,
};
use vmlayer_proto::guest::v1 as guest;
use vmlayer_proto::v1 as pb;

use crate::api::{self, err};
use crate::guest::Agent;
use crate::net::{self, Lease, Pool};
use crate::process::{RemoteProcess, RemotePty};
use crate::vm::{self, Vm};
use crate::{Config, BACKEND_NAME};

/// Written next to a snapshot so restore knows what the files are.
#[derive(Serialize, Deserialize)]
struct SnapshotMeta {
    vmstate: String,
    mem: String,
    root: String,
    /// vol{i}.ext4 file names in mount order, with their guest paths.
    volumes: Vec<(String, String)>,
    spec: SandboxSpec,
}

/// Written into the jail so `attach` and `list` can rebuild a handle.
#[derive(Serialize, Deserialize)]
struct JailMeta {
    spec: SandboxSpec,
    created_at: SystemTime,
    mounts: Vec<(String, String)>,
}

pub struct FirecrackerSandbox {
    config: Arc<Config>,
    pool: Arc<Mutex<Pool>>,
    id: SandboxId,
    spec: SandboxSpec,
    lease: Lease,
    vm: Mutex<Vm>,
    agent: Agent,
    /// Mount order as given to Firecracker: (device, guest path).
    mounts: Vec<(String, String)>,
    exposed: Mutex<HashMap<u16, net::Proxy>>,
}

fn device_for(i: usize) -> String {
    // vda is the root; volumes follow in drive order.
    format!("/dev/vd{}", (b'b' + i as u8) as char)
}

impl FirecrackerSandbox {
    /// Populate a fresh jail: kernel, root disk (CoW clone of `base`),
    /// volume disks (hard-linked: the VM writes the volume itself).
    async fn populate_jail(
        config: &Config,
        jail: &Path,
        root_src: &Path,
        root_is_clone: bool,
        mounts: &[Mount],
    ) -> Result<Vec<(String, String)>> {
        tokio::fs::create_dir_all(jail).await?;
        vm::link_or_clone(&config.kernel, &jail.join(vm::KERNEL)).await?;
        let root = jail.join(vm::ROOT_DISK);
        vm::clone_file(root_src, &root).await?;
        if root_is_clone {
            if let Some(agent) = &config.guest_agent {
                vm::inject_agent(&root, agent).await?;
            }
        }
        let mut out = Vec::new();
        for (i, m) in mounts.iter().enumerate() {
            let src = match &m.source {
                VolumeSource::BlockImage { path } => path,
                VolumeSource::Dir { .. } => {
                    return Err(Error::Unsupported {
                        backend: BACKEND_NAME,
                        feature: "directory volumes (use block-image volumes)",
                    })
                }
            };
            let dst = jail.join(vm::vol_disk(i));
            tokio::fs::hard_link(src, &dst).await.map_err(|e| {
                err(format!(
                    "volume images must live on the same filesystem as the data dir ({}): {e}",
                    src.display()
                ))
            })?;
            out.push((device_for(i), m.guest_path.clone()));
        }
        Ok(out)
    }

    async fn configure_and_start(
        vmm: &Vm,
        config: &Config,
        spec: &SandboxSpec,
        lease: &Lease,
        mounts: &[Mount],
    ) -> Result<()> {
        let api = &vmm.api;
        api.machine_config(spec.vcpus, spec.memory_mib).await?;
        api.boot_source(
            &format!("/{}", vm::KERNEL),
            &vm::boot_args(config, &spec.id.0),
        )
        .await?;
        api.drive(&api::Drive {
            drive_id: "rootfs",
            path_on_host: &format!("/{}", vm::ROOT_DISK),
            is_root_device: true,
            is_read_only: false,
        })
        .await?;
        for (i, m) in mounts.iter().enumerate() {
            let p = format!("/{}", vm::vol_disk(i));
            api.drive(&api::Drive {
                drive_id: &format!("vol{i}"),
                path_on_host: &p,
                is_root_device: false,
                is_read_only: m.read_only,
            })
            .await?;
        }
        if spec.network != vmlayer_core::NetworkMode::None {
            api.network_interface("eth0", net::TAP, net::GUEST_MAC)
                .await?;
        }
        api.vsock(3, &format!("/{}", vm::VSOCK)).await?;
        let _ = lease;
        api.start().await
    }

    async fn write_jail_meta(jail: &Path, meta: &JailMeta) -> Result<()> {
        let json = serde_json::to_vec_pretty(meta).map_err(|e| err(e.to_string()))?;
        tokio::fs::write(jail.join("vmlayer.json"), json).await?;
        Ok(())
    }

    pub async fn boot(
        config: Arc<Config>,
        pool: Arc<Mutex<Pool>>,
        spec: SandboxSpec,
        base: PathBuf,
        lease: Lease,
    ) -> Result<Self> {
        let jail = config.jail_root(&spec.id);
        let result: Result<Self> = async {
            let mounts = Self::populate_jail(&config, &jail, &base, true, &spec.mounts).await?;
            net::create_netns(&lease, config.jail_uid, config.jail_gid).await?;
            let vmm = Vm::launch(&config, &spec.id.0, &lease).await?;
            Self::configure_and_start(&vmm, &config, &spec, &lease, &spec.mounts).await?;
            let agent = Agent::wait_ready(
                jail.join(vm::VSOCK),
                config.agent_port,
                Duration::from_secs(10),
            )
            .await?;
            let mut agent = agent;
            for (dev, path) in &mounts {
                let ro = spec
                    .mounts
                    .iter()
                    .find(|m| &m.guest_path == path)
                    .map(|m| m.read_only)
                    .unwrap_or(false);
                agent
                    .guest
                    .mount(guest::MountRequest {
                        source: dev.clone(),
                        path: path.clone(),
                        fstype: "ext4".into(),
                        read_only: ro,
                    })
                    .await
                    .map_err(|e| err(format!("mounting {dev} at {path}: {e}")))?;
            }
            if let Some(entry) = &spec.entrypoint {
                agent
                    .guest
                    .start_entrypoint(guest::EntrypointRequest {
                        argv: entry.argv.clone(),
                        cwd: entry.cwd.clone().unwrap_or_default(),
                        env: entry
                            .env
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    })
                    .await
                    .map_err(|e| err(format!("starting entrypoint: {e}")))?;
            }
            let created_at = SystemTime::now();
            Self::write_jail_meta(
                &jail,
                &JailMeta {
                    spec: spec.clone(),
                    created_at,
                    mounts: mounts.clone(),
                },
            )
            .await?;
            Ok(Self {
                config: config.clone(),
                pool: pool.clone(),
                id: spec.id.clone(),
                spec,
                lease: lease.clone(),
                vm: Mutex::new(vmm),
                agent,
                mounts,
                exposed: Mutex::new(HashMap::new()),
            })
        }
        .await;
        if result.is_err() {
            net::delete_netns(&lease).await;
            pool.lock().await.release(lease.index);
            let _ = tokio::fs::remove_dir_all(&jail).await;
        }
        result
    }

    pub async fn restore(
        config: Arc<Config>,
        pool: Arc<Mutex<Pool>>,
        spec: SandboxSpec,
        snapshot: &SnapshotRef,
        lease: Lease,
    ) -> Result<Self> {
        let snap_dir = PathBuf::from(&snapshot.engine_ref);
        let meta: SnapshotMeta =
            serde_json::from_slice(&tokio::fs::read(snap_dir.join("meta.json")).await?)
                .map_err(|e| err(format!("snapshot meta: {e}")))?;
        let jail = config.jail_root(&spec.id);
        let result: Result<Self> = async {
            // Root disk: CoW clone of the snapshot's; no agent injection
            // (it is already inside). Volumes: the daemon's fresh clones.
            let mounts = Self::populate_jail(
                &config,
                &jail,
                &snap_dir.join(&meta.root),
                false,
                &spec.mounts,
            )
            .await?;
            vm::link_or_clone(&snap_dir.join(&meta.vmstate), &jail.join("vmstate")).await?;
            vm::link_or_clone(&snap_dir.join(&meta.mem), &jail.join("mem")).await?;
            net::create_netns(&lease, config.jail_uid, config.jail_gid).await?;
            let vmm = Vm::launch(&config, &spec.id.0, &lease).await?;
            vmm.api
                .snapshot_load(&api::SnapshotLoad {
                    snapshot_path: "/vmstate",
                    mem_backend: api::MemBackend {
                        backend_type: "File",
                        backend_path: "/mem",
                    },
                    enable_diff_snapshots: false,
                    resume_vm: true,
                    network_overrides: vec![api::NetworkOverride {
                        iface_id: "eth0",
                        host_dev_name: net::TAP,
                    }],
                })
                .await?;
            // The guest resumed with its old vsock connections dead; reconnect.
            let agent = Agent::wait_ready(
                jail.join(vm::VSOCK),
                config.agent_port,
                Duration::from_secs(10),
            )
            .await?;
            let created_at = SystemTime::now();
            Self::write_jail_meta(
                &jail,
                &JailMeta {
                    spec: spec.clone(),
                    created_at,
                    mounts: mounts.clone(),
                },
            )
            .await?;
            Ok(Self {
                config: config.clone(),
                pool: pool.clone(),
                id: spec.id.clone(),
                spec,
                lease: lease.clone(),
                vm: Mutex::new(vmm),
                agent,
                mounts,
                exposed: Mutex::new(HashMap::new()),
            })
        }
        .await;
        if result.is_err() {
            net::delete_netns(&lease).await;
            pool.lock().await.release(lease.index);
            let _ = tokio::fs::remove_dir_all(&jail).await;
        }
        result
    }

    pub async fn attach(
        config: Arc<Config>,
        pool: Arc<Mutex<Pool>>,
        id: &SandboxId,
    ) -> Result<Self> {
        let jail = config.jail_root(id);
        let meta: JailMeta = serde_json::from_slice(
            &tokio::fs::read(jail.join("vmlayer.json"))
                .await
                .map_err(|_| Error::SandboxNotFound(id.clone()))?,
        )
        .map_err(|e| err(format!("jail meta: {e}")))?;
        let index = Vm::lease_index(&jail).ok_or_else(|| Error::SandboxNotFound(id.clone()))?;
        let lease = pool.lock().await.claim(index, id);
        let vmm = Vm::attach(&config, &id.0)
            .await
            .map_err(|_| Error::SandboxNotFound(id.clone()))?;
        let agent = Agent::wait_ready(
            jail.join(vm::VSOCK),
            config.agent_port,
            Duration::from_secs(5),
        )
        .await?;
        Ok(Self {
            config: config.clone(),
            pool,
            id: id.clone(),
            spec: meta.spec,
            lease,
            vm: Mutex::new(vmm),
            agent,
            mounts: meta.mounts,
            exposed: Mutex::new(HashMap::new()),
        })
    }

    pub async fn list(config: &Config) -> Result<Vec<SandboxInfo>> {
        let base = config.data_dir.join("jail").join("firecracker");
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(&base).await else {
            return Ok(out);
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let jail = entry.path().join("root");
            let Ok(bytes) = tokio::fs::read(jail.join("vmlayer.json")).await else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<JailMeta>(&bytes) else {
                continue;
            };
            let state = match api::Client::new(jail.join(vm::API_SOCK)).info().await {
                Ok(i) if i.state == "Paused" => SandboxState::Paused,
                Ok(_) => SandboxState::Running,
                Err(_) => SandboxState::Stopped,
            };
            out.push(SandboxInfo {
                id: meta.spec.id.clone(),
                state,
                backend: BACKEND_NAME.to_string(),
                created_at: meta.created_at,
                labels: meta.spec.labels.clone(),
            });
        }
        Ok(out)
    }

    fn exec_pb(req: &ExecRequest) -> pb::ExecRequest {
        pb::ExecRequest {
            argv: req.argv.clone(),
            cwd: req.cwd.clone().unwrap_or_default(),
            env: req
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            timeout: req
                .timeout
                .and_then(|d| prost_types::Duration::try_from(d).ok()),
            ..Default::default()
        }
    }
}

#[async_trait]
impl SandboxHandle for FirecrackerSandbox {
    fn id(&self) -> &SandboxId {
        &self.id
    }

    fn backend(&self) -> &'static str {
        BACKEND_NAME
    }

    async fn state(&self) -> Result<SandboxState> {
        match self.vm.lock().await.api.info().await {
            Ok(i) if i.state == "Paused" => Ok(SandboxState::Paused),
            Ok(i) if i.state == "Running" => Ok(SandboxState::Running),
            Ok(_) => Ok(SandboxState::Creating),
            Err(_) => Ok(SandboxState::Stopped),
        }
    }

    async fn exec(&self, req: ExecRequest) -> Result<Box<dyn Process>> {
        Ok(Box::new(
            RemoteProcess::spawn(self.agent.clone(), Self::exec_pb(&req)).await?,
        ))
    }

    async fn pty(&self, req: ExecRequest) -> Result<Box<dyn Pty>> {
        let size = req
            .pty
            .unwrap_or(vmlayer_core::PtySize { cols: 80, rows: 24 });
        let open = pb::PtyOpen {
            argv: req.argv.clone(),
            cwd: req.cwd.clone().unwrap_or_default(),
            env: req
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            size: Some(pb::PtySize {
                cols: size.cols.into(),
                rows: size.rows.into(),
            }),
            ..Default::default()
        };
        Ok(Box::new(RemotePty::open(self.agent.clone(), open).await?))
    }

    async fn processes(&self) -> Result<Vec<ProcessInfo>> {
        let mut agent = self.agent.clone();
        let r = agent
            .processes
            .list(pb::ListProcessesRequest::default())
            .await
            .map_err(|e| err(e.to_string()))?;
        Ok(r.into_inner()
            .processes
            .into_iter()
            .map(|p| ProcessInfo {
                pid: p.pid,
                argv: p.argv,
                started_at: p
                    .started_at
                    .and_then(|t| SystemTime::try_from(t).ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                is_entrypoint: p.is_entrypoint,
            })
            .collect())
    }

    async fn process(&self, pid: u32) -> Result<Box<dyn Process>> {
        Ok(Box::new(
            RemoteProcess::attach(self.agent.clone(), pid).await?,
        ))
    }

    async fn wait_for_port(&self, port: u16, timeout: Duration) -> Result<()> {
        let mut agent = self.agent.clone();
        agent
            .guest
            .wait_for_port(guest::WaitForPortRequest {
                port: port.into(),
                timeout: prost_types::Duration::try_from(timeout).ok(),
            })
            .await
            .map(|_| ())
            .map_err(|e| match e.code() {
                tonic::Code::DeadlineExceeded => Error::Timeout(timeout),
                _ => err(e.to_string()),
            })
    }

    async fn read_file(&self, path: &str) -> Result<Bytes> {
        let mut agent = self.agent.clone();
        let mut stream = agent
            .files
            .read(pb::ReadFileRequest {
                path: path.into(),
                ..Default::default()
            })
            .await
            .map_err(|e| Error::Guest(e.message().to_string()))?
            .into_inner();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(
                &chunk
                    .map_err(|e| Error::Guest(e.message().to_string()))?
                    .data,
            );
        }
        Ok(Bytes::from(buf))
    }

    async fn write_file(&self, path: &str, data: Bytes, mode: u32) -> Result<()> {
        let mut agent = self.agent.clone();
        let header = pb::WriteFileRequest {
            part: Some(pb::write_file_request::Part::Header(
                pb::write_file_request::Header {
                    path: path.into(),
                    mode,
                    ..Default::default()
                },
            )),
        };
        let chunks: Vec<pb::WriteFileRequest> = data
            .chunks(64 * 1024)
            .map(|c| pb::WriteFileRequest {
                part: Some(pb::write_file_request::Part::Data(c.to_vec())),
            })
            .collect();
        agent
            .files
            .write(tokio_stream::iter(std::iter::once(header).chain(chunks)))
            .await
            .map(|_| ())
            .map_err(|e| Error::Guest(e.message().to_string()))
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<FileInfo>> {
        let mut agent = self.agent.clone();
        let r = agent
            .files
            .list(pb::ListFilesRequest {
                path: path.into(),
                ..Default::default()
            })
            .await
            .map_err(|e| Error::Guest(e.message().to_string()))?;
        Ok(r.into_inner()
            .entries
            .into_iter()
            .map(|f| FileInfo {
                path: f.path,
                size: f.size,
                is_dir: f.is_dir,
                mode: f.mode,
                modified_at: f
                    .modified_at
                    .and_then(|t| SystemTime::try_from(t).ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
            })
            .collect())
    }

    async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        let mut agent = self.agent.clone();
        agent
            .files
            .remove(pb::RemoveRequest {
                path: path.into(),
                recursive,
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(|e| Error::Guest(e.message().to_string()))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let mut agent = self.agent.clone();
        agent
            .files
            .mkdir(pb::MkdirRequest {
                path: path.into(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(|e| Error::Guest(e.message().to_string()))
    }

    async fn snapshot(&self, opts: SnapshotOptions) -> Result<SnapshotRef> {
        let id = SnapshotId(format!("snap-{}", Uuid::now_v7().simple()));
        let dir = self.config.snapshot_dir(&id.0);
        tokio::fs::create_dir_all(&dir).await?;
        let vmm = self.vm.lock().await;
        let was_paused = matches!(vmm.api.info().await.map(|i| i.state), Ok(s) if s == "Paused");
        // Quiesce guest filesystems, pause the vCPUs, write memory + state.
        let mut agent = self.agent.clone();
        let _ = agent.guest.freeze_fs(pb::Empty {}).await;
        if !was_paused {
            vmm.api.pause().await?;
        }
        let result: Result<()> = async {
            vmm.api
                .snapshot_create("/snap-vmstate", "/snap-mem", false)
                .await?;
            // Move the files out of the jail into the snapshot dir and clone the disks.
            tokio::fs::rename(vmm.jail_root.join("snap-vmstate"), dir.join("vmstate")).await?;
            tokio::fs::rename(vmm.jail_root.join("snap-mem"), dir.join("mem")).await?;
            vm::clone_file(&vmm.jail_root.join(vm::ROOT_DISK), &dir.join(vm::ROOT_DISK)).await?;
            Ok(())
        }
        .await;
        if !was_paused && !opts.stay_paused {
            let _ = vmm.api.resume().await;
            let _ = agent.guest.thaw_fs(pb::Empty {}).await;
        }
        result?;
        let meta = SnapshotMeta {
            vmstate: "vmstate".into(),
            mem: "mem".into(),
            root: vm::ROOT_DISK.into(),
            volumes: self
                .mounts
                .iter()
                .enumerate()
                .map(|(i, (_, p))| (vm::vol_disk(i), p.clone()))
                .collect(),
            spec: self.spec.clone(),
        };
        tokio::fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta).map_err(|e| err(e.to_string()))?,
        )
        .await?;
        let size = tokio::fs::metadata(dir.join("mem"))
            .await
            .map(|m| m.len())
            .unwrap_or(0)
            + tokio::fs::metadata(dir.join(vm::ROOT_DISK))
                .await
                .map(|m| m.len())
                .unwrap_or(0);
        let engine_version = vmm
            .api
            .info()
            .await
            .map(|i| i.vmm_version)
            .unwrap_or_default();
        Ok(SnapshotRef {
            id,
            backend: BACKEND_NAME.to_string(),
            engine_ref: dir.display().to_string(),
            spec: self.spec.clone(),
            engine_version,
            cpu_model: cpu_model(),
            created_at: SystemTime::now(),
            size_bytes: size,
            includes_memory: true,
            volumes: Vec::new(),
            labels: opts.labels,
        })
    }

    /// Fork = snapshot + N restores sharing the memory file copy-on-write.
    async fn fork(
        &self,
        count: u32,
        mounts: Vec<Vec<Mount>>,
    ) -> Result<Vec<Box<dyn SandboxHandle>>> {
        let snap = self.snapshot(SnapshotOptions::default()).await?;
        let mut children: Vec<Box<dyn SandboxHandle>> = Vec::new();
        for i in 0..count as usize {
            let mut spec = self.spec.clone();
            spec.id = SandboxId(format!("sb-{}", Uuid::now_v7().simple()));
            spec.mounts = mounts.get(i).cloned().unwrap_or_default();
            let lease = self.pool.lock().await.allocate(&spec.id)?;
            let child =
                Self::restore(self.config.clone(), self.pool.clone(), spec, &snap, lease).await?;
            children.push(Box::new(child));
        }
        Ok(children)
    }

    async fn freeze_fs(&self) -> Result<()> {
        let mut agent = self.agent.clone();
        agent
            .guest
            .freeze_fs(pb::Empty {})
            .await
            .map(|_| ())
            .map_err(|e| err(e.to_string()))
    }

    async fn thaw_fs(&self) -> Result<()> {
        let mut agent = self.agent.clone();
        agent
            .guest
            .thaw_fs(pb::Empty {})
            .await
            .map(|_| ())
            .map_err(|e| err(e.to_string()))
    }

    async fn pause(&self) -> Result<()> {
        self.vm.lock().await.api.pause().await
    }

    async fn resume(&self) -> Result<()> {
        self.vm.lock().await.api.resume().await
    }

    async fn kill(&self) -> Result<()> {
        self.exposed.lock().await.clear();
        let mut vmm = self.vm.lock().await;
        vmm.kill().await?;
        net::delete_netns(&self.lease).await;
        self.pool.lock().await.release(self.lease.index);
        let _ = tokio::fs::remove_dir_all(vmm.jail_root.parent().unwrap_or(&vmm.jail_root)).await;
        Ok(())
    }

    async fn attach(&self, _mount: Mount) -> Result<()> {
        Err(Error::Unsupported {
            backend: BACKEND_NAME,
            feature: "hot attach (needs PCI mode)",
        })
    }

    async fn detach(&self, _guest_path: &str) -> Result<()> {
        Err(Error::Unsupported {
            backend: BACKEND_NAME,
            feature: "hot detach",
        })
    }

    async fn expose(&self, guest_port: u16) -> Result<Endpoint> {
        let mut exposed = self.exposed.lock().await;
        if let Some(p) = exposed.get(&guest_port) {
            return Ok(Endpoint {
                host: "127.0.0.1".into(),
                port: p.port,
            });
        }
        let proxy = net::proxy(self.lease.ns_ip, guest_port).await?;
        let port = proxy.port;
        exposed.insert(guest_port, proxy);
        Ok(Endpoint {
            host: "127.0.0.1".into(),
            port,
        })
    }

    async fn unexpose(&self, guest_port: u16) -> Result<()> {
        self.exposed.lock().await.remove(&guest_port);
        Ok(())
    }
}

/// Host CPU model, recorded with memory snapshots (they do not restore on a
/// different microarchitecture).
fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    text.lines()
        .find(|l| l.starts_with("model name") || l.starts_with("CPU part"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
}

#[allow(dead_code)]
fn _labels(m: &BTreeMap<String, String>) -> usize {
    m.len()
}
