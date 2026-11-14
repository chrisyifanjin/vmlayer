//! Volume attachment types. The daemon owns volumes; backends receive these.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ids::VolumeId;

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
