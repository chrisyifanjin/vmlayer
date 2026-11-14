//! Shared value types. All of these cross the daemon boundary, so they derive
//! `serde` and avoid backend-specific fields.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(SandboxId);
id_type!(SnapshotId);
id_type!(VolumeId);
id_type!(VolumeSnapshotId);

/// What a backend can do. One field per optional method on
/// [`crate::SandboxHandle`]. The daemon surfaces this to SDK users unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub snapshot: bool,
    /// Snapshots include guest RAM, so restore resumes running processes.
    /// `false` means filesystem-only snapshots (Docker).
    pub memory_snapshot: bool,
    pub fork: bool,
    pub pause: bool,
    pub pty: bool,
    pub expose_port: bool,
    /// Can mount [`Mount`]s given in [`SandboxSpec`].
    pub volumes: bool,
    /// Can attach/detach volumes on a running sandbox. No v1 backend.
    pub hot_attach: bool,
}

impl Capabilities {
    /// What a microVM backend (Firecracker, libkrun) is expected to offer.
    pub const MICROVM: Self = Self {
        snapshot: true,
        memory_snapshot: true,
        fork: true,
        pause: true,
        pty: true,
        expose_port: true,
        volumes: true,
        hot_attach: false,
    };

    /// What a container backend offers: good enough for local dev and CI.
    pub const CONTAINER: Self = Self {
        snapshot: true,
        memory_snapshot: false,
        fork: false,
        pause: true,
        pty: true,
        expose_port: true,
        volumes: true,
        hot_attach: false,
    };
}

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

/// Host-side storage behind one volume attachment. Produced by the daemon's
/// volume store (which owns creation, copy-on-write cloning, snapshots and
/// upload); backends only plumb it into the guest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VolumeSource {
    /// A raw ext4 image file, attached as a virtio-blk device. Firecracker
    /// and libkrun.
    BlockImage { path: PathBuf },
    /// A host directory, bind-mounted. Docker.
    Dir { path: PathBuf },
}

/// One volume attachment. The guest agent performs the mount at handshake
/// from a table the daemon sends; images need no `fstab`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    pub volume: VolumeId,
    pub source: VolumeSource,
    /// Absolute path inside the guest.
    pub guest_path: String,
    pub read_only: bool,
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

/// A command to run inside the sandbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    /// `argv[0]` is the program. `vmlayerd` turns a shell string from the SDK
    /// into `["/bin/sh", "-c", s]` before it gets here.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Kill the process if it runs longer than this.
    pub timeout: Option<Duration>,
    /// Allocate a pty instead of pipes. Output then arrives as a single
    /// stream with terminal escapes intact.
    pub pty: Option<PtySize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// One chunk of process output, in arrival order.
#[derive(Clone, Debug)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub data: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Exit code, or `None` if killed by a signal.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub duration: Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub mode: u32,
    pub modified_at: SystemTime,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotOptions {
    pub labels: BTreeMap<String, String>,
    /// Leave the sandbox paused after the snapshot instead of resuming it.
    pub stay_paused: bool,
}

/// A filesystem-only snapshot of one volume. Cheap, portable across hosts
/// and backends, exportable to the blob store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeSnapshotRef {
    pub id: VolumeSnapshotId,
    pub volume: VolumeId,
    /// Where it was mounted when taken, if it was attached.
    pub guest_path: Option<String>,
    pub created_at: SystemTime,
    pub size_bytes: u64,
    /// Uploaded to the configured blob store.
    pub durable: bool,
    pub labels: BTreeMap<String, String>,
}

/// A persisted sandbox snapshot: memory (on microVM backends), root overlay,
/// and a volume snapshot for every mount that was attached. Taken atomically
/// while paused. Lives in the daemon's store, outlives its sandbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRef {
    pub id: SnapshotId,
    pub backend: &'static str,
    pub created_at: SystemTime,
    pub size_bytes: u64,
    pub includes_memory: bool,
    pub volumes: Vec<VolumeSnapshotRef>,
    pub labels: BTreeMap<String, String>,
}

/// Per-restore overrides. Identity (id, network address) is always fresh,
/// and every volume comes back as a fresh copy-on-write clone (the daemon
/// prepares these and passes them as `mounts`); everything else defaults to
/// what the snapshot recorded.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RestoreOptions {
    pub id: Option<SandboxId>,
    pub ttl: Option<Duration>,
    /// Cloned volumes to attach in place of the ones recorded in the
    /// snapshot, one per `SnapshotRef::volumes` entry, same guest paths.
    pub mounts: Vec<Mount>,
    pub labels: BTreeMap<String, String>,
}

/// A host-reachable address for a guest port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}
