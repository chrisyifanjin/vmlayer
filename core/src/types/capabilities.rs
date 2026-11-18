//! What a backend can do.

use serde::{Deserialize, Serialize};

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
    /// Can mount [`crate::Mount`]s given in [`crate::SandboxSpec`].
    pub volumes: bool,
    /// Can attach/detach volumes on a running sandbox. No v1 backend.
    pub hot_attach: bool,
    /// Enforces [`crate::EgressPolicy`] allowlists.
    #[serde(default)]
    pub egress_policy: bool,
    /// Enforces [`crate::IoLimits`] (network and disk rate limits).
    #[serde(default)]
    pub io_limits: bool,
    /// Enforces fencing deadlines outside the daemon
    /// ([`crate::SandboxHandle::set_fencing_deadline`]).
    #[serde(default)]
    pub fencing: bool,
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
        egress_policy: true,
        io_limits: true,
        fencing: true,
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
        egress_policy: false,
        io_limits: false,
        fencing: false,
    };
}
