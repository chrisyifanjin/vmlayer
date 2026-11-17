//! Boot-time description of a sandbox and its observable state.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::ids::SandboxId;
use super::process::ExecRequest;
use super::volume::Mount;

/// The root filesystem a sandbox boots from. `vmlayerd` produces this from an
/// OCI reference; backends only consume it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Rootfs {
    /// A flat ext4 image on the host. Firecracker and libkrun mount it as the
    /// root block device. Always opened copy-on-write; the base is never
    /// written to.
    Ext4 { path: PathBuf },
    /// An unpacked directory tree. libkrun can serve it over virtio-fs;
    /// Docker turns it into an image.
    Dir { path: PathBuf },
    /// An image already present in the engine's own store, by its digest
    /// (Docker). Firecracker/libkrun backends reject this with
    /// [`crate::Error::Unsupported`].
    EngineImage { digest: String },
}

/// Everything needed to boot one sandbox. Produced by `vmlayerd` after image
/// resolution and policy checks; backends treat it as read-only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub id: SandboxId,
    pub rootfs: Rootfs,
    /// Volumes to attach at boot. The daemon has already enforced the
    /// single-writer rule; backends attach what they are given.
    pub mounts: Vec<Mount>,
    pub vcpus: u32,
    pub memory_mib: u32,
    /// Writable overlay size. `None` lets the backend choose.
    pub disk_mib: Option<u32>,
    pub env: BTreeMap<String, String>,
    pub workdir: String,
    /// Started by the guest agent as soon as the sandbox is up; the daemon
    /// has already resolved `AUTO` to the image's `ENTRYPOINT`/`CMD`. `None`
    /// starts nothing. Reported as pid 1 in `SandboxHandle::processes`.
    pub entrypoint: Option<ExecRequest>,
    pub network: NetworkMode,
    /// Outbound allowlist. `None` allows all egress except cloud metadata
    /// endpoints; `Some` drops everything the policy does not allow.
    #[serde(default)]
    pub egress: Option<EgressPolicy>,
    /// Network and disk rate limits.
    #[serde(default)]
    pub limits: IoLimits,
    /// Hard wall-clock lifetime. The backend kills the sandbox when it
    /// elapses. `None` means unbounded.
    pub ttl: Option<Duration>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkMode {
    /// No network device at all.
    None,
    /// Outbound only, NAT through the host. The default.
    Egress,
    /// Outbound plus the ability to `expose` guest ports.
    EgressAndExpose,
}

/// Where a sandbox may connect. Everything not allowed is dropped. DNS is
/// answered only for allowed names (decision 0012).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressPolicy {
    pub allow: Vec<EgressRule>,
}

impl EgressPolicy {
    /// Parse rules such as `10.0.0.0/8`, `1.2.3.4:5432`, `pypi.org`,
    /// `*.pythonhosted.org:443` or `github.com:22,443`.
    pub fn parse<S: AsRef<str>>(rules: &[S]) -> std::result::Result<Self, String> {
        let allow = rules
            .iter()
            .map(|r| r.as_ref().parse())
            .collect::<std::result::Result<_, _>>()?;
        Ok(Self { allow })
    }

    /// The domain rules that allow `name`, if any. `name` may end in a dot.
    pub fn domain_rules<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a EgressRule> {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        self.allow.iter().filter(move |r| r.matches_name(&name))
    }
}

/// One allowlist entry. An empty `ports` list means every port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EgressRule {
    /// An IPv4 address or network.
    Network {
        addr: std::net::Ipv4Addr,
        prefix: u8,
        ports: Vec<u16>,
    },
    /// A DNS name. With `wildcard`, every name *under* it (`*.example.com`
    /// matches `a.example.com` and `a.b.example.com`, not `example.com`).
    Domain {
        name: String,
        wildcard: bool,
        ports: Vec<u16>,
    },
}

impl EgressRule {
    pub fn ports(&self) -> &[u16] {
        match self {
            Self::Network { ports, .. } | Self::Domain { ports, .. } => ports,
        }
    }

    fn matches_name(&self, name: &str) -> bool {
        match self {
            Self::Domain {
                name: n,
                wildcard: false,
                ..
            } => name == n,
            Self::Domain {
                name: n,
                wildcard: true,
                ..
            } => {
                name.len() > n.len() + 1 && name.ends_with(n.as_str()) && {
                    name.as_bytes()[name.len() - n.len() - 1] == b'.'
                }
            }
            Self::Network { .. } => false,
        }
    }
}

impl std::str::FromStr for EgressRule {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        let bad = |why: &str| format!("egress rule {s:?}: {why}");
        if s.contains('[') || s.matches(':').count() > 1 {
            return Err(bad("IPv6 is not supported; sandboxes have IPv4 only"));
        }
        let (target, ports) = match s.rsplit_once(':') {
            Some((t, p)) => {
                let ports = p
                    .split(',')
                    .map(|x| x.trim().parse::<u16>().ok().filter(|&n| n != 0))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| bad("ports must be numbers from 1 to 65535"))?;
                (t, ports)
            }
            None => (s, Vec::new()),
        };
        if target.is_empty() {
            return Err(bad("empty target"));
        }
        let (host, prefix) = match target.split_once('/') {
            Some((h, p)) => (
                h,
                Some(
                    p.parse::<u8>()
                        .ok()
                        .filter(|&n| n <= 32)
                        .ok_or_else(|| bad("prefix must be 0 to 32"))?,
                ),
            ),
            None => (target, None),
        };
        if let Ok(addr) = host.parse::<std::net::Ipv4Addr>() {
            let prefix = prefix.unwrap_or(32);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            let addr = std::net::Ipv4Addr::from(u32::from(addr) & mask);
            return Ok(Self::Network {
                addr,
                prefix,
                ports,
            });
        }
        if prefix.is_some() {
            return Err(bad("a prefix needs an IPv4 address"));
        }
        let lower = host.to_ascii_lowercase();
        let (wildcard, name) = match lower.strip_prefix("*.") {
            Some(rest) => (true, rest.to_string()),
            None => (false, lower),
        };
        let name = name.trim_end_matches('.').to_string();
        let valid = !name.is_empty()
            && name.len() <= 253
            && name.contains('.')
            && name.split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
        if !valid {
            return Err(bad("not an IPv4 address, network, or DNS name"));
        }
        Ok(Self::Domain {
            name,
            wildcard,
            ports,
        })
    }
}

impl std::fmt::Display for EgressRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network {
                addr, prefix: 32, ..
            } => write!(f, "{addr}")?,
            Self::Network { addr, prefix, .. } => write!(f, "{addr}/{prefix}")?,
            Self::Domain {
                name,
                wildcard: true,
                ..
            } => write!(f, "*.{name}")?,
            Self::Domain { name, .. } => write!(f, "{name}")?,
        }
        let ports = self.ports();
        if !ports.is_empty() {
            let list: Vec<String> = ports.iter().map(u16::to_string).collect();
            write!(f, ":{}", list.join(","))?;
        }
        Ok(())
    }
}

/// Rate limits on a sandbox's devices. `None` means unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoLimits {
    /// Network bandwidth in megabits per second, each direction.
    pub network_mbps: Option<u32>,
    /// Disk bandwidth in megabytes per second, per disk.
    pub disk_mbps: Option<u32>,
    /// Disk operations per second, per disk.
    pub disk_iops: Option<u32>,
}

impl IoLimits {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxState {
    Creating,
    Running,
    Paused,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub id: SandboxId,
    pub state: SandboxState,
    pub backend: String,
    pub created_at: SystemTime,
    pub labels: BTreeMap<String, String>,
}

/// A host-reachable address for a guest port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_rules() {
        for (input, shown) in [
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("10.1.2.3/8", "10.0.0.0/8"),
            ("1.2.3.4", "1.2.3.4"),
            ("1.2.3.4:5432", "1.2.3.4:5432"),
            ("PyPI.org", "pypi.org"),
            ("*.pythonhosted.org:443", "*.pythonhosted.org:443"),
            ("github.com:22,443", "github.com:22,443"),
            ("example.com.", "example.com"),
        ] {
            let rule: EgressRule = input.parse().unwrap();
            assert_eq!(rule.to_string(), shown, "{input}");
        }
        for bad in [
            "",
            "::1",
            "[::1]:443",
            "10.0.0.0/33",
            "host:0",
            "host:http",
            "localhost",
            "-bad.com",
            "a..b",
            "pypi.org/16",
            "*.com.",
        ] {
            assert!(
                bad.parse::<EgressRule>().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn domain_matching() {
        let p = EgressPolicy::parse(&["pypi.org", "*.pythonhosted.org:443", "10.0.0.0/8"]).unwrap();
        let hits = |n: &str| p.domain_rules(n).count();
        assert_eq!(hits("pypi.org"), 1);
        assert_eq!(hits("PYPI.ORG."), 1);
        assert_eq!(hits("www.pypi.org"), 0);
        assert_eq!(hits("files.pythonhosted.org"), 1);
        assert_eq!(hits("a.b.pythonhosted.org"), 1);
        assert_eq!(hits("pythonhosted.org"), 0);
        assert_eq!(hits("evilpythonhosted.org"), 0);
        assert_eq!(hits("example.com"), 0);
    }
}
