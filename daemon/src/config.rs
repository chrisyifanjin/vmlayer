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
    pub log: LogConfig,
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
