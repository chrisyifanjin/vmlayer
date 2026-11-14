//! Sandbox and volume snapshots, and restore options.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::ids::{SandboxId, SnapshotId, VolumeId, VolumeSnapshotId};
use super::spec::SandboxSpec;
use super::volume::Mount;

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
    /// Opaque backend handle for the snapshot data (an image id for Docker,
    /// a directory for Firecracker). Only the backend that made it reads it.
    pub engine_ref: String,
    /// The spec the sandbox was created with. Restore reuses it, with
    /// `RestoreOptions` overriding identity, ttl, mounts and labels.
    pub spec: SandboxSpec,
    /// Exact engine release that produced it (e.g. `firecracker 1.16.1`).
    /// Snapshots are not portable across releases; restore checks this.
    pub engine_version: String,
    /// Host CPU model the memory snapshot was taken on. Restore on a
    /// different microarchitecture fails with `SnapshotIncompatible`.
    pub cpu_model: Option<String>,
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
