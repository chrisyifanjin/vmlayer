//! Firecracker backend: one jailed microVM per sandbox.
//!
//! Exercised with Firecracker 1.18 on aarch64 nested KVM. The backend keeps
//! one jailed microVM per sandbox and can restore from memory snapshots.
//!
//! Layout on the host (`<data_dir>/firecracker/`):
//!
//! ```text
//!   jail/firecracker/<sandbox-id>/root/   the jailer chroot
//!     vmlinux            kernel (hard link)
//!     root.ext4          per-sandbox copy-on-write clone of the image
//!     vol0.ext4 ...      volume block images, in mount order
//!     v.sock             vsock unix socket (host side)
//!     api.sock           Firecracker API
//!   snapshots/<snapshot-id>/  vmstate, mem, root.ext4, vol*.ext4, meta.json
//! ```
//!
//! Drive paths inside the chroot are fixed names, so a snapshot restored
//! into a fresh jail finds its disks at the same paths without any PATCH.
//! Memory is loaded from the snapshot file as a private mapping, so N
//! restores of one snapshot share pages copy-on-write: that is `fork`.

mod api;
mod guest;
mod net;
mod process;
mod sandbox;
mod template;
mod vm;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use vmlayer_core::{
    Backend, Capabilities, Error, ImageResolver, ResolvedImage, RestoreOptions, Result, Rootfs,
    SandboxHandle, SandboxId, SandboxInfo, SandboxSpec, SnapshotRef, VolumeSourceKind,
};

pub use net::Subnet;
pub use sandbox::FirecrackerSandbox;

/// `firecracker --version`, for the daemon's backend listing.
pub async fn engine_version(bin: &Path) -> Result<String> {
    api::version(bin).await
}

pub const BACKEND_NAME: &str = "firecracker";

/// Everything the backend needs to know about the host. The daemon fills it
/// from its config; the installer probes most of it.
#[derive(Clone, Debug)]
pub struct Config {
    pub firecracker_bin: PathBuf,
    pub jailer_bin: PathBuf,
    /// Uncompressed kernel image (`vmlinux`).
    pub kernel: PathBuf,
    /// Extra kernel command line appended to the default.
    pub kernel_args: String,
    /// Root of the backend's state: jails and snapshots.
    pub data_dir: PathBuf,
    /// uid/gid the jailer drops to.
    pub jail_uid: u32,
    pub jail_gid: u32,
    /// Host-side address pool for the per-VM veth pairs (one /30 each).
    pub subnet: net::Subnet,
    /// Nameservers pushed to guests.
    pub dns: Vec<String>,
    /// vsock port the guest agent listens on.
    pub agent_port: u32,
    /// Static guest agent binary to inject into root images that lack one.
    pub guest_agent: Option<PathBuf>,
    /// Keep a data-free snapshot on disk for eligible cold creates.
    pub template_snapshots: bool,
    /// Optional command run before freezing the data-free template.
    pub template_warmup_argv: Vec<String>,
}

impl Config {
    pub fn jail_root(&self, id: &SandboxId) -> PathBuf {
        self.data_dir
            .join("jail")
            .join("firecracker")
            .join(&id.0)
            .join("root")
    }

    pub fn snapshot_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("snapshots").join(id)
    }
}

pub struct FirecrackerBackend {
    config: Arc<Config>,
    net: Arc<Mutex<net::Pool>>,
    template_lock: Arc<Mutex<()>>,
}

impl FirecrackerBackend {
    pub fn new(config: Config) -> Self {
        let pool = net::Pool::new(config.subnet);
        Self {
            config: Arc::new(config),
            net: Arc::new(Mutex::new(pool)),
            template_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Checks that do not need KVM: binaries, kernel, data dir.
    pub fn preflight(&self) -> Result<()> {
        for (what, p) in [
            ("firecracker binary", &self.config.firecracker_bin),
            ("jailer binary", &self.config.jailer_bin),
            ("kernel image", &self.config.kernel),
        ] {
            if !p.exists() {
                return Err(Error::Engine {
                    backend: BACKEND_NAME,
                    message: format!("{what} not found at {}", p.display()),
                    source: None,
                });
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Backend for FirecrackerBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::MICROVM
    }

    fn volume_source_kind(&self) -> VolumeSourceKind {
        VolumeSourceKind::BlockImage
    }

    async fn probe(&self) -> Result<()> {
        self.preflight()?;
        if !Path::new("/dev/kvm").exists() {
            return Err(Error::Engine {
                backend: BACKEND_NAME,
                message: "/dev/kvm not present: this host has no KVM (bare metal or nested virtualization required)".into(),
                source: None,
            });
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(|e| Error::Engine {
                backend: BACKEND_NAME,
                message: format!(
                    "/dev/kvm not accessible: {e} (run as root or add the user to the kvm group)"
                ),
                source: None,
            })?;
        net::ensure_host(self.config.subnet).await?;
        api::version(&self.config.firecracker_bin).await?;
        // A daemon restart can interrupt a background template build. These
        // VMs are never registered as user sandboxes, so recover and remove
        // them before reserving surviving network namespaces.
        let template_jails = self.config.data_dir.join("jail/firecracker");
        if let Ok(mut entries) = tokio::fs::read_dir(&template_jails).await {
            while let Some(entry) = entries.next_entry().await? {
                let id = entry.file_name();
                let id = id.to_string_lossy();
                if !id.starts_with("template-") {
                    continue;
                }
                let jail_root = self.config.jail_root(&SandboxId(id.to_string()));
                let index = vm::Vm::lease_index(&jail_root);
                match vm::Vm::attach(&self.config, &id).await {
                    Ok(mut vmm) => {
                        vmm.kill().await?;
                    }
                    Err(error) => {
                        let pid = tokio::fs::read_to_string(jail_root.join(vm::PID_FILE))
                            .await
                            .ok()
                            .and_then(|text| text.trim().parse::<u32>().ok());
                        if pid.is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists()) {
                            tracing::warn!(%id, %error, "template VM may still be running; retaining its network lease");
                            continue;
                        }
                    }
                }
                if let Some(index) = index {
                    let lease = self.net.lock().await.lease_for(index);
                    net::delete_netns(&lease).await;
                }
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            }
        }
        // Namespaces left by a previous daemon belong to VMs we may reattach;
        // keep their indices out of the pool until they are released.
        let mut pool = self.net.lock().await;
        for index in net::existing_netns().await {
            pool.claim(index, &SandboxId(format!("recovered-{index}")));
        }
        Ok(())
    }

    async fn create(&self, spec: SandboxSpec) -> Result<Box<dyn SandboxHandle>> {
        let base = match &spec.rootfs {
            Rootfs::Ext4 { path } => path.clone(),
            _ => {
                return Err(Error::Unsupported {
                    backend: BACKEND_NAME,
                    feature: "non-ext4 rootfs",
                })
            }
        };
        let eligible = self.config.template_snapshots && spec.mounts.is_empty();
        if eligible {
            let path = self.template_ref_path(&spec, &base);
            if let Some(snapshot) = self.cached_template(&path).await {
                let lease = self.net.lock().await.allocate(&spec.id)?;
                match FirecrackerSandbox::restore_template(
                    self.config.clone(),
                    self.net.clone(),
                    spec.clone(),
                    &snapshot,
                    lease,
                )
                .await
                {
                    Ok(sb) => return Ok(Box::new(sb)),
                    Err(e) => {
                        tracing::warn!(error = %e, "template restore failed; cold booting");
                        let _ = tokio::fs::remove_file(path).await;
                    }
                }
            }
        }
        let template_spec = eligible.then(|| spec.clone());
        let template_base = base.clone();
        let lease = self.net.lock().await.allocate(&spec.id)?;
        let sb = FirecrackerSandbox::boot(self.config.clone(), self.net.clone(), spec, base, lease)
            .await?;
        if let Some(template_spec) = template_spec {
            let config = self.config.clone();
            let net = self.net.clone();
            let template_lock = self.template_lock.clone();
            tokio::spawn(async move {
                // Let the first caller run an initial command before the
                // one-time template build competes for CPU on this host.
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let backend = FirecrackerBackend {
                    config,
                    net,
                    template_lock,
                };
                if let Err(e) = backend.template_for(&template_spec, &template_base).await {
                    tracing::warn!(error = %e, "background template build failed");
                }
            });
        }
        Ok(Box::new(sb))
    }

    async fn restore(
        &self,
        snapshot: &SnapshotRef,
        opts: RestoreOptions,
    ) -> Result<Box<dyn SandboxHandle>> {
        if snapshot.backend != BACKEND_NAME {
            return Err(Error::SnapshotIncompatible {
                id: snapshot.id.clone(),
                reason: format!("taken by backend {}", snapshot.backend),
            });
        }
        let mut spec = snapshot.spec.clone();
        if let Some(id) = opts.id {
            spec.id = id;
        }
        spec.ttl = opts.ttl.or(spec.ttl);
        spec.mounts = opts.mounts;
        spec.labels.extend(opts.labels);
        let lease = self.net.lock().await.allocate(&spec.id)?;
        let sb = FirecrackerSandbox::restore(
            self.config.clone(),
            self.net.clone(),
            spec,
            snapshot,
            lease,
        )
        .await?;
        Ok(Box::new(sb))
    }

    async fn attach(&self, id: &SandboxId) -> Result<Box<dyn SandboxHandle>> {
        let sb = FirecrackerSandbox::attach(self.config.clone(), self.net.clone(), id).await?;
        Ok(Box::new(sb))
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        FirecrackerSandbox::list(&self.config).await
    }

    async fn delete_snapshot(&self, snapshot: &SnapshotRef) -> Result<()> {
        let dir = PathBuf::from(&snapshot.engine_ref);
        if dir.starts_with(self.config.data_dir.join("snapshots")) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        Ok(())
    }
}

/// Image resolution for microVMs is the daemon's OCI pipeline (roadmap step
/// 3). Until it exists, an image reference is a path to an ext4 file.
pub struct Ext4PathResolver;

#[async_trait]
impl ImageResolver for Ext4PathResolver {
    async fn resolve(&self, image: &str) -> Result<ResolvedImage> {
        let path = PathBuf::from(image);
        if !path.is_absolute() || !path.exists() {
            return Err(Error::Guest(format!(
                "the firecracker backend currently takes an absolute path to an ext4 root image, got {image:?}"
            )));
        }
        Ok(ResolvedImage {
            rootfs: Rootfs::Ext4 { path },
            entrypoint: None,
            workdir: None,
            env: Default::default(),
        })
    }
}
