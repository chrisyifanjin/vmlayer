//! Shared value types. All of these cross the daemon boundary, so they derive
//! `serde` and avoid backend-specific fields. One submodule per concern;
//! everything is re-exported flat so callers write `vmlayer_core::SandboxSpec`.

pub mod capabilities;
pub mod ids;
pub mod process;
pub mod snapshot;
pub mod spec;
pub mod volume;

pub use capabilities::Capabilities;
pub use ids::{SandboxId, SnapshotId, VolumeId, VolumeSnapshotId};
pub use process::{
    ExecRequest, ExitStatus, FileInfo, OutputChunk, OutputStream, ProcessInfo, PtySize,
};
pub use snapshot::{RestoreOptions, SnapshotOptions, SnapshotRef, VolumeSnapshotRef};
pub use spec::{Endpoint, NetworkMode, Rootfs, SandboxInfo, SandboxSpec, SandboxState};
pub use volume::{Mount, VolumeSource};
