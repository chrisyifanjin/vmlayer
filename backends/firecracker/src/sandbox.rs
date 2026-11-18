//! One sandbox = one jailed Firecracker VM plus a gRPC client to its guest agent.

use std::collections::BTreeMap;
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
use crate::dns::{AllowSink, Allowed};
use crate::guest::{Agent, Link};
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
    link: Arc<Link>,
    /// Mount order as given to Firecracker: (device, guest path).
    mounts: Vec<(String, String)>,
    /// The VM's shim: owns its data-path services (decision 0019).
    shim: crate::shim::Client,
}

/// Upper bound on the volume flush that precedes a kill.
const VOLUME_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// PATH for guest processes when neither the image nor the caller sets one.
/// The name every sandbox resolves to the host (decision 0022).
pub const HOST_NAME: &str = "vmlayer.host";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

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
        if root_is_clone {
            // Boot path: clone the image with the guest agent already in it.
            let prepared = vm::prepared_image(config, root_src).await?;
            vm::clone_file(&prepared, &root).await?;
        } else {
            // Restore path: the snapshot's disk already contains the agent.
            vm::clone_file(root_src, &root).await?;
        }
        let mut out = Vec::new();
        for (i, m) in mounts.iter().enumerate() {
            let src = match &m.source {
                VolumeSource::BlockImage { path } => path,
                // The shim creates this drive (a block device) before boot.
                VolumeSource::LazyBlockImage { .. } => {
                    out.push((device_for(i), m.guest_path.clone()));
                    continue;
                }
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
        // Firecracker drops to the jail uid: it must own the jail root (it
        // creates sockets and snapshot files there) and every disk it opens.
        vm::chown_for_jail(jail, config.jail_uid, config.jail_gid)?;
        vm::chown_for_jail(&root, config.jail_uid, config.jail_gid)?;
        for (i, m) in mounts.iter().enumerate() {
            if matches!(m.source, VolumeSource::LazyBlockImage { .. }) {
                continue;
            }
            vm::chown_for_jail(
                &jail.join(vm::vol_disk(i)),
                config.jail_uid,
                config.jail_gid,
            )?;
        }
        Ok(out)
    }

    /// Tear down what a failed boot or restore left behind, keeping its
    /// console log under `<data>/failed/` so the failure can be diagnosed.
    async fn discard_failed(config: &Config, id: &SandboxId, jail: &Path) {
        // Stop the VMM if it got as far as starting.
        if let Ok(mut vmm) = Vm::attach(config, &id.0).await {
            let _ = vmm.kill().await;
        }
        let keep = config.data_dir.join("failed");
        if tokio::fs::create_dir_all(&keep).await.is_ok() {
            let _ = tokio::fs::copy(
                vm::console_log_path(jail),
                keep.join(format!("{id}.console.log")),
            )
            .await;
        }
        let _ = tokio::fs::remove_dir_all(jail.parent().unwrap_or(jail)).await;
    }

    /// Make what the guest has written to its volumes durable before the VM
    /// goes away. A volume outlives its sandbox but the guest's page cache
    /// does not: without this, a file written just before `kill` would be
    /// missing from the volume. Freezing flushes each volume's filesystem and
    /// leaves it consistent on its disk image. Best effort and bounded, so a
    /// hung guest cannot hold up its own removal.
    async fn flush_volumes(&self) {
        if self.mounts.is_empty() {
            return;
        }
        let flush = async {
            let state = self.vm.lock().await.api.info().await.map(|i| i.state);
            if matches!(&state, Ok(s) if s == "Paused") {
                // A paused guest cannot flush; let it run for a moment.
                self.resume().await?;
            }
            self.freeze_fs().await
        };
        match tokio::time::timeout(VOLUME_FLUSH_TIMEOUT, flush).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(id = %self.id, %error, "could not flush volumes before kill")
            }
            Err(_) => tracing::warn!(id = %self.id, "timed out flushing volumes before kill"),
        }
    }

    /// The shim's DNS proxy that enforces `spec.egress`, if there is one.
    fn dns_spec(
        config: &Config,
        spec: &SandboxSpec,
        lease: &Lease,
    ) -> Result<Option<crate::shim::DnsSpec>> {
        let Some(egress) = spec.egress.clone() else {
            return Ok(None);
        };
        if spec.network == vmlayer_core::NetworkMode::None {
            return Ok(None);
        }
        let upstreams = config
            .dns
            .iter()
            .map(|d| {
                d.parse::<std::net::IpAddr>()
                    .map(|ip| std::net::SocketAddr::new(ip, 53))
                    .map_err(|e| err(format!("dns server {d:?}: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(crate::shim::DnsSpec {
            listen: std::net::SocketAddr::new(lease.host_ip.into(), net::DNS_PORT),
            netns: lease.netns.clone(),
            upstreams,
            egress,
        }))
    }

    /// Lazy volumes the shim serves, at their drive paths in the jail.
    fn lazy_specs(jail: &Path, mounts: &[Mount]) -> Vec<crate::shim::LazySpec> {
        mounts
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match &m.source {
                VolumeSource::LazyBlockImage {
                    path,
                    snapshot,
                    store_url,
                    store_options,
                } => Some(crate::shim::LazySpec {
                    node: jail.join(vm::vol_disk(i)),
                    image: path.clone(),
                    snapshot: snapshot.clone(),
                    store_url: store_url.clone(),
                    store_options: store_options.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Connect to the guest agent; on failure include the console tail.
    async fn agent_ready(config: &Config, jail: &Path, settle: bool) -> Result<Agent> {
        match Agent::wait_ready(
            jail.join(vm::VSOCK),
            config.agent_port,
            config.boot_timeout,
            settle,
        )
        .await
        {
            Ok(a) => Ok(a),
            Err(e) => {
                let tail = vm::console_tail(jail).await;
                Err(err(format!("{e}; guest console: {tail}")))
            }
        }
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
            rate_limiter: api::RateLimiter::disk(&spec.limits),
        })
        .await?;
        for (i, m) in mounts.iter().enumerate() {
            let p = format!("/{}", vm::vol_disk(i));
            api.drive(&api::Drive {
                drive_id: &format!("vol{i}"),
                path_on_host: &p,
                is_root_device: false,
                is_read_only: m.read_only,
                rate_limiter: api::RateLimiter::disk(&spec.limits),
            })
            .await?;
        }
        if spec.network != vmlayer_core::NetworkMode::None {
            api.network_interface(
                "eth0",
                net::TAP,
                net::GUEST_MAC,
                api::RateLimiter::network(&spec.limits),
            )
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

    async fn configure_guest(
        agent: &mut Agent,
        spec: &SandboxSpec,
        mounts: &[(String, String)],
        host_ip: std::net::Ipv4Addr,
    ) -> Result<()> {
        for (dev, path) in mounts {
            if path.is_empty() {
                continue; // a template's placeholder drive: never mounted
            }
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
        let mut env: std::collections::HashMap<String, String> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env.entry("PATH".into())
            .or_insert_with(|| DEFAULT_PATH.into());
        agent
            .guest
            .configure(guest::ConfigureRequest {
                env,
                cwd: spec.workdir.clone(),
                hostname: spec.id.0.clone(),
                // The host as this guest reaches it (its veth peer).
                hosts: [(HOST_NAME.to_string(), host_ip.to_string())].into(),
            })
            .await
            .map_err(|e| err(format!("configuring guest: {e}")))?;
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
        let spec_id = spec.id.clone();
        let result: Result<Self> = async {
            let t0 = std::time::Instant::now();
            let mounts = Self::populate_jail(&config, &jail, &base, true, &spec.mounts).await?;
            let t_disks = t0.elapsed();
            net::create_netns(
                &lease,
                config.jail_uid,
                config.jail_gid,
                spec.egress.as_ref(),
                &config.host_ports,
            )
            .await?;
            let services = vm::Services {
                dns: Self::dns_spec(&config, &spec, &lease)?,
                lazy: Self::lazy_specs(&jail, &spec.mounts),
                ..Default::default()
            };
            let t_net = t0.elapsed();
            let vmm = Vm::launch(&config, &spec.id.0, &lease, services).await?;
            let t_vmm = t0.elapsed();
            Self::configure_and_start(&vmm, &config, &spec, &lease, &spec.mounts).await?;
            let t_start = t0.elapsed();
            let mut agent = Self::agent_ready(&config, &jail, false).await?;
            tracing::info!(
                id = %spec.id,
                disks_ms = t_disks.as_millis() as u64,
                netns_ms = (t_net - t_disks).as_millis() as u64,
                vmm_ms = (t_vmm - t_net).as_millis() as u64,
                configure_ms = (t_start - t_vmm).as_millis() as u64,
                guest_boot_ms = (t0.elapsed() - t_start).as_millis() as u64,
                "microVM booted"
            );
            Self::configure_guest(&mut agent, &spec, &mounts, lease.host_ip).await?;
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
            let vmm_shim = vmm.shim.clone();
            Ok(Self {
                config: config.clone(),
                pool: pool.clone(),
                id: spec.id.clone(),
                spec,
                lease: lease.clone(),
                vm: Mutex::new(vmm),
                link: Arc::new(Link::new(agent)),
                mounts,
                shim: vmm_shim,
            })
        }
        .await;
        if result.is_err() {
            Self::discard_failed(&config, &spec_id, &jail).await;
            net::delete_netns(&lease).await;
            pool.lock().await.release(lease.index);
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
        let spec_id = spec.id.clone();
        let result: Result<Self> = async {
            // Root disk: CoW clone of the snapshot's; no agent injection
            // (it is already inside). Volumes: the daemon's fresh clones.
            // The guest's block devices are fixed by the snapshot (vdb is
            // whatever was vol0 then), so attach the new volumes in the
            // snapshot's order, matched by guest path.
            let ordered: Vec<Mount> = meta
                .volumes
                .iter()
                .map(|(_, path)| {
                    // A template's placeholder drive (no guest path) takes
                    // the sandbox's workspace volume (review A4).
                    let found = if path.is_empty() {
                        spec.mounts.iter().find(|m| m.guest_path == spec.workdir)
                    } else {
                        spec.mounts.iter().find(|m| &m.guest_path == path)
                    };
                    found
                        .cloned()
                        .ok_or_else(|| err(format!("restore is missing a volume for {path:?}")))
                })
                .collect::<Result<_>>()?;
            let mounts =
                Self::populate_jail(&config, &jail, &snap_dir.join(&meta.root), false, &ordered)
                    .await?;

            vm::link_or_clone(&snap_dir.join(&meta.vmstate), &jail.join("vmstate")).await?;
            vm::link_or_clone(&snap_dir.join(&meta.mem), &jail.join("mem")).await?;
            for f in ["vmstate", "mem"] {
                vm::chown_for_jail(&jail.join(f), config.jail_uid, config.jail_gid)?;
            }
            net::create_netns(
                &lease,
                config.jail_uid,
                config.jail_gid,
                spec.egress.as_ref(),
                &config.host_ports,
            )
            .await?;
            // Ordered like the snapshot's drives (see `ordered`).
            let services = vm::Services {
                dns: Self::dns_spec(&config, &spec, &lease)?,
                lazy: Self::lazy_specs(&jail, &ordered),
                uffd: (cfg!(target_os = "linux") && config.prefetch_memory).then(|| {
                    crate::shim::UffdSpec {
                        socket: jail.join("uffd.sock"),
                        mem: jail.join("mem"),
                    }
                }),
                ..Default::default()
            };
            let mem_backend = if services.uffd.is_some() {
                api::MemBackend {
                    backend_type: "Uffd",
                    backend_path: "/uffd.sock",
                }
            } else {
                api::MemBackend {
                    backend_type: "File",
                    backend_path: "/mem",
                }
            };
            let vmm = Vm::launch(&config, &spec.id.0, &lease, services).await?;
            vmm.api
                .snapshot_load(&api::SnapshotLoad {
                    snapshot_path: "/vmstate",
                    mem_backend,
                    enable_diff_snapshots: false,
                    resume_vm: true,
                    network_overrides: Vec::new(),
                })
                .await?;
            // The guest resumed with its old vsock connections dead; reconnect.
            // The snapshot was taken with volumes frozen; let writes flow
            // again. Must not be skipped, so retry on a fresh connection.
            let mut agent = Self::agent_ready(&config, &jail, true).await?;
            let mut attempt = 0;
            loop {
                let call =
                    tokio::time::timeout(Duration::from_secs(2), agent.guest.thaw_fs(pb::Empty {}));
                match call.await {
                    Ok(Ok(_)) => break,
                    _ if attempt < 5 => {
                        attempt += 1;
                        agent = Self::agent_ready(&config, &jail, true).await?;
                    }
                    Ok(Err(e)) => {
                        return Err(err(format!(
                            "thawing volumes after restore: {}",
                            e.message()
                        )))
                    }
                    Err(_) => {
                        return Err(err(
                            "thawing volumes after restore: no answer from the agent",
                        ))
                    }
                }
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
            let vmm_shim = vmm.shim.clone();
            Ok(Self {
                config: config.clone(),
                pool: pool.clone(),
                id: spec.id.clone(),
                spec,
                lease: lease.clone(),
                vm: Mutex::new(vmm),
                link: Arc::new(Link::new(agent)),
                mounts,
                shim: vmm_shim,
            })
        }
        .await;
        if result.is_err() {
            Self::discard_failed(&config, &spec_id, &jail).await;
            net::delete_netns(&lease).await;
            pool.lock().await.release(lease.index);
        }
        result
    }

    /// Resume the data-free template, then apply this sandbox's own defaults
    /// and entrypoint. A normal user snapshot resumes its existing processes
    /// and must never start the entrypoint a second time.
    pub async fn restore_template(
        config: Arc<Config>,
        pool: Arc<Mutex<Pool>>,
        spec: SandboxSpec,
        snapshot: &SnapshotRef,
        lease: Lease,
    ) -> Result<Self> {
        let sandbox = Self::restore(config, pool, spec, snapshot, lease).await?;
        let configured = async {
            // The workspace took the placeholder drive's place: have
            // Firecracker reopen it so the guest sees its real size, then
            // mount it (review A4).
            if !sandbox.mounts.is_empty() {
                let vmm = sandbox.vm.lock().await;
                for (i, _) in sandbox.mounts.iter().enumerate() {
                    vmm.api
                        .update_drive(&format!("vol{i}"), &format!("/{}", vm::vol_disk(i)))
                        .await?;
                }
            }
            let mut agent = sandbox.link.agent();
            Self::configure_guest(
                &mut agent,
                &sandbox.spec,
                &sandbox.mounts,
                sandbox.lease.host_ip,
            )
            .await
        };
        if let Err(error) = configured.await {
            let _ = sandbox.kill().await;
            return Err(error);
        }
        Ok(sandbox)
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
            true,
        )
        .await?;
        // The shim kept the VM's services (DNS, proxies, lazy volumes) going.
        let vmm_shim = vmm.shim.clone();
        Ok(Self {
            config: config.clone(),
            pool,
            id: id.clone(),
            spec: meta.spec,
            lease,
            vm: Mutex::new(vmm),
            link: Arc::new(Link::new(agent)),
            mounts: meta.mounts,
            shim: vmm_shim,
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

    /// Install a fresh agent connection. Needed after every snapshot:
    /// Firecracker resets the guest's vsock connections when it takes one.
    async fn reconnect(&self) -> Result<()> {
        let jail = self.config.jail_root(&self.id);
        let fresh = Self::agent_ready(&self.config, &jail, true).await?;
        self.link.replace(fresh);
        Ok(())
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
            user: req.user.clone().unwrap_or_default(),
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
            RemoteProcess::spawn(self.link.clone(), Self::exec_pb(&req)).await?,
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
            user: req.user.clone().unwrap_or_default(),
            ..Default::default()
        };
        Ok(Box::new(RemotePty::open(self.link.agent(), open).await?))
    }

    async fn processes(&self) -> Result<Vec<ProcessInfo>> {
        let mut agent = self.link.agent();
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
            RemoteProcess::attach(self.link.clone(), pid).await?,
        ))
    }

    async fn wait_for_port(&self, port: u16, timeout: Duration) -> Result<()> {
        let mut agent = self.link.agent();
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
        let mut agent = self.link.agent();
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
        let mut agent = self.link.agent();
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
        let mut agent = self.link.agent();
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
        let mut agent = self.link.agent();
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
        let mut agent = self.link.agent();
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
        let mut agent = self.link.agent();
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
        let resumed = !was_paused && !opts.stay_paused;
        if resumed {
            let _ = vmm.api.resume().await;
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
        drop(vmm);
        if resumed {
            // The snapshot reset the agent connection; reconnect and thaw.
            self.reconnect().await?;
            let mut agent = self.link.agent();
            let _ = agent.guest.thaw_fs(pb::Empty {}).await;
        }
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
            durable: false,
        })
    }

    async fn freeze_fs(&self) -> Result<()> {
        let mut agent = self.link.agent();
        agent
            .guest
            .freeze_fs(pb::Empty {})
            .await
            .map(|_| ())
            .map_err(|e| err(e.to_string()))
    }

    async fn thaw_fs(&self) -> Result<()> {
        let mut agent = self.link.agent();
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
        self.vm.lock().await.api.resume().await?;
        // If the pause was for a snapshot, the agent connection was reset and
        // the volumes are frozen: reconnect, then let writes through again.
        self.reconnect().await?;
        let mut agent = self.link.agent();
        agent
            .guest
            .thaw_fs(pb::Empty {})
            .await
            .map(|_| ())
            .map_err(|e| err(format!("thawing volumes after resume: {}", e.message())))
    }

    async fn kill(&self) -> Result<()> {
        self.flush_volumes().await;
        self.link.mark_dead();
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
        let r = self
            .shim
            .call(&crate::shim::Request::Expose { port: guest_port })
            .await?;
        Ok(Endpoint {
            host: "127.0.0.1".into(),
            port: r.host_port.ok_or_else(|| err("shim returned no port"))?,
        })
    }

    async fn unexpose(&self, guest_port: u16) -> Result<()> {
        self.shim
            .call(&crate::shim::Request::Unexpose { port: guest_port })
            .await
            .map(|_| ())
    }

    async fn set_fencing_deadline(&self, deadline: Option<SystemTime>) -> Result<()> {
        let deadline_ms = deadline.map(|d| {
            d.duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_millis() as u64)
                .unwrap_or(0)
        });
        self.shim
            .call(&crate::shim::Request::Extend { deadline_ms })
            .await
            .map(|_| ())
    }
}

/// Host CPU model, recorded with memory snapshots (they do not restore on a
/// different microarchitecture).
pub(crate) fn cpu_model() -> Option<String> {
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

/// Sends addresses the DNS proxy learned to a VM namespace's nftables sets.
pub(crate) struct NftSink(pub(crate) String);

#[async_trait::async_trait]
impl AllowSink for NftSink {
    async fn allow(&self, entries: &[Allowed]) -> Result<()> {
        net::allow_resolved(&self.0, entries).await
    }
}
