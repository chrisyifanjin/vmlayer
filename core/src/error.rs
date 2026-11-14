use crate::types::{SandboxId, SnapshotId, VolumeId};

pub type Result<T> = std::result::Result<T, Error>;

/// Every failure a backend can report. `vmlayerd` maps these 1:1 onto gRPC
/// status codes, and the SDKs map them onto their exception types, so adding a
/// variant here is an API change.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The backend does not implement this feature. Check [`crate::Capabilities`] first.
    #[error("backend {backend} does not support {feature}")]
    Unsupported {
        backend: &'static str,
        feature: &'static str,
    },

    #[error("sandbox {0} not found")]
    SandboxNotFound(SandboxId),

    #[error("snapshot {0} not found")]
    SnapshotNotFound(SnapshotId),

    #[error("volume {0} not found")]
    VolumeNotFound(VolumeId),

    /// Single-writer rule: the volume is already attached read-write.
    #[error("volume {volume} is attached read-write to sandbox {holder}")]
    VolumeBusy { volume: VolumeId, holder: SandboxId },

    /// The sandbox exists but is in the wrong state for this call
    /// (e.g. `exec` on a paused sandbox, `resume` on a running one).
    #[error("sandbox {id} is {actual}, expected {expected}")]
    InvalidState {
        id: SandboxId,
        expected: &'static str,
        actual: &'static str,
    },

    /// A snapshot cannot run on this host: CPU model, guest kernel or backend
    /// differs from where it was taken.
    #[error("snapshot {id} is incompatible with this host: {reason}")]
    SnapshotIncompatible { id: SnapshotId, reason: String },

    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// The host cannot fit the request (memory, vCPUs, disk, address space).
    #[error("insufficient resources: {0}")]
    ResourceExhausted(String),

    /// The guest agent reported an error (bad path, permission denied, ...).
    #[error("guest: {0}")]
    Guest(String),

    /// The engine process (firecracker, dockerd, libkrun) failed.
    #[error("{backend}: {message}")]
    Engine {
        backend: &'static str,
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
