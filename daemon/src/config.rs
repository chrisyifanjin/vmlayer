//! Daemon configuration: a TOML file, overridden by flags. Defaults work
//! with no file at all.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `unix:///path/to.sock` or `tcp://host:port`.
    pub listen: String,
    /// Required for TCP listeners; ignored for unix sockets.
    pub token: Option<String>,
    pub data_dir: PathBuf,
    /// Backend used for `backend = "auto"`; `None` picks the strongest
    /// available (firecracker, libkrun, docker).
    pub default_backend: Option<String>,
    pub defaults: Defaults,
    pub docker: DockerConfig,
    pub firecracker: FirecrackerConfig,
    pub blob_store: BlobStoreConfig,
    pub cluster: ClusterConfig,
    /// Refuse sandboxes the host cannot hold (review A3).
    pub admission: crate::admission::AdmissionConfig,
    pub log: LogConfig,
}

/// Several daemons sharing one blob store (decision 0014).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterConfig {
    /// Unique name of this host; defaults to the hostname.
    pub host_id: Option<String>,
    /// Address other machines reach this daemon at, e.g. `tcp://10.0.0.5:7070`.
    /// Set it to publish a heartbeat in the blob store.
    pub advertise: Option<String>,
    /// Published with the heartbeat, for placement constraints.
    pub labels: std::collections::BTreeMap<String, String>,
    pub heartbeat_secs: u64,
    /// Lease lifetime; a crashed holder blocks a name for at most this long.
    pub lease_ttl_secs: u64,
    /// Bring back rescheduled sandboxes whose host died (decision 0015).
    pub rescue: bool,
    /// How often a rescheduled sandbox's workspace is checkpointed.
    pub checkpoint_secs: u64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            host_id: None,
            advertise: None,
            labels: Default::default(),
            heartbeat_secs: 10,
            lease_ttl_secs: 30,
            rescue: false,
            checkpoint_secs: 60,
        }
    }
}

/// Object storage for durable volume snapshots (ADR 0011). Off unless `url`
/// is set. Credentials come from the environment (`AWS_*`, `AZURE_*`,
/// `GOOGLE_*`) or the machine's cloud identity, never from this file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BlobStoreConfig {
    /// `s3://bucket/prefix`, `az://container/prefix`, `gs://bucket/prefix`
    /// or `file:///shared/dir`.
    pub url: Option<String>,
    /// Store settings, e.g. `endpoint`, `region`, `allow_http`,
    /// `use_emulator`. Override the environment.
    pub options: std::collections::BTreeMap<String, String>,
    /// Unreferenced chunks younger than this survive garbage collection.
    /// Default 24.
    pub gc_grace_hours: Option<u64>,
    /// Requests in flight per transfer. Default 16.
    pub concurrency: Option<usize>,
    /// Make block-image volumes restored from the store usable at once,
    /// downloading on demand (decision 0016; needs the `nbd` kernel module,
    /// else the volume is downloaded before it is attached). Default true.
    pub lazy_restore: Option<bool>,
}

impl BlobStoreConfig {
    pub fn open(&self) -> anyhow::Result<Option<vmlayer_blob::Remote>> {
        let Some(url) = self.url.as_deref().filter(|u| !u.is_empty()) else {
            return Ok(None);
        };
        let remote = vmlayer_blob::Remote::open(url, &self.options)?
            .with_gc_grace(std::time::Duration::from_secs(
                self.gc_grace_hours.unwrap_or(24) * 3600,
            ))
            .with_concurrency(self.concurrency.unwrap_or(16));
        Ok(Some(remote))
    }
}

/// Firecracker backend. Disabled unless `enabled = true`; the daemon then
/// probes for KVM, the binaries and the kernel and reports why it is
/// unavailable if any is missing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FirecrackerConfig {
    pub enabled: bool,
    pub firecracker_bin: PathBuf,
    pub jailer_bin: PathBuf,
    /// Uncompressed `vmlinux` built with the Firecracker guest config.
    pub kernel: PathBuf,
    pub kernel_args: String,
    /// Static guest agent injected into root images (`vmlayer-guest-agent`).
    pub guest_agent: Option<PathBuf>,
    pub jail_uid: u32,
    pub jail_gid: u32,
    /// Host-side pool for per-VM veth pairs, e.g. "10.200.0.0/16".
    pub host_subnet: String,
    pub dns: Vec<String>,
    pub agent_port: u32,
    /// Restore eligible sandboxes from an on-disk, data-free VM template.
    pub template_snapshots: bool,
    /// Admin-controlled, data-free command to populate guest page cache.
    pub template_warmup_argv: Vec<String>,
    /// Templates kept on disk (one per image and VM shape), most recently
    /// used first. Each takes the guest's memory size plus its root disk.
    pub template_limit: usize,
    /// The per-VM helper (`vmlayer-shim`); default: next to `vmlayerd`.
    pub shim_bin: Option<PathBuf>,
    /// Copy all of a restored guest's memory into place right away
    /// (userfaultfd) instead of faulting it in on first touch.
    pub prefetch_memory: bool,
    /// Cold boots that may run at once; more creates queue. 0 = one per CPU.
    pub max_concurrent_boots: usize,
    /// Seconds a booted VM has to bring up its guest agent.
    pub boot_timeout_secs: u64,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            firecracker_bin: "/usr/local/bin/firecracker".into(),
            jailer_bin: "/usr/local/bin/jailer".into(),
            kernel: "/var/lib/vmlayer/vmlinux".into(),
            kernel_args: String::new(),
            guest_agent: Some("/usr/local/bin/vmlayer-guest-agent".into()),
            jail_uid: 65534,
            jail_gid: 65534,
            host_subnet: "10.200.0.0/16".into(),
            dns: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            agent_port: 5200,
            template_snapshots: false,
            template_warmup_argv: Vec::new(),
            template_limit: 4,
            shim_bin: None,
            prefetch_memory: true,
            max_concurrent_boots: 0,
            boot_timeout_secs: 30,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub vcpus: u32,
    pub memory_mib: u32,
    pub volume_size_mib: u64,
    pub workdir: String,
    /// Per-process output kept for re-attach, in bytes.
    pub output_buffer_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    pub enabled: bool,
    pub host: Option<String>,
    pub network: String,
    pub socat_image: String,
    pub probe_image: String,
    /// Check at startup that the data dir is visible inside containers
    /// (matters for VM-based engines on macOS and Windows).
    pub verify_mounts: bool,
    /// Maximum processes per sandbox; 0 disables the limit.
    pub pids_limit: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// `pretty` or `json`.
    pub format: String,
    /// A `tracing` filter, e.g. `info,vmlayerd=debug`.
    pub filter: String,
}

pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VMLAYER_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(unix)]
    if unsafe_is_root() {
        return PathBuf::from("/var/lib/vmlayer");
    }
    dirs_home().join(".vmlayer")
}

pub fn default_socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("VMLAYER_SOCKET") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("vmlayer").join("vmlayerd.sock");
    }
    default_data_dir().join("vmlayerd.sock")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(unix)]
fn unsafe_is_root() -> bool {
    // Reading our own uid cannot fail.
    unsafe { libc_geteuid() == 0 }
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: format!("unix://{}", default_socket_path().display()),
            token: None,
            data_dir: default_data_dir(),
            default_backend: None,
            defaults: Defaults::default(),
            docker: DockerConfig::default(),
            firecracker: FirecrackerConfig::default(),
            blob_store: BlobStoreConfig::default(),
            cluster: ClusterConfig::default(),
            admission: Default::default(),
            log: LogConfig::default(),
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            vcpus: 1,
            memory_mib: 512,
            volume_size_mib: 8192,
            workdir: "/work".into(),
            output_buffer_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: None,
            network: "bridge".into(),
            socat_image: "alpine/socat:1.8.0.0".into(),
            probe_image: "alpine:3.20".into(),
            verify_mounts: true,
            pids_limit: 4096,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: "pretty".into(),
            filter: "info".into(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
                Ok(toml::from_str(&text)?)
            }
            None => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_toml_fills_defaults() {
        let c: Config = toml::from_str("data_dir = \"/tmp/x\"\n[defaults]\nvcpus = 4\n").unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/tmp/x"));
        assert_eq!(c.defaults.vcpus, 4);
        assert_eq!(c.defaults.memory_mib, 512);
        assert!(c.docker.enabled);
    }
}
