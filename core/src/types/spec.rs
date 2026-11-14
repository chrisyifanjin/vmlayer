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
    pub backend: &'static str,
    pub created_at: SystemTime,
    pub labels: BTreeMap<String, String>,
}

/// A host-reachable address for a guest port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}
