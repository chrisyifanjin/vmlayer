//! The backend contract.
//!
//! Three traits, three lifetimes:
//!
//! * [`Backend`] lives as long as the daemon. One instance per engine.
//! * [`SandboxHandle`] lives as long as one sandbox.
//! * [`Process`] / [`Pty`] live as long as one command inside a sandbox.
//!
//! Everything is `async` and object-safe (`dyn`), because `vmlayerd` holds a
//! `HashMap<&str, Box<dyn Backend>>` and picks at runtime.

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use tokio::io::AsyncWrite;

use crate::error::Result;
use crate::types::*;

/// Streaming output of a process or pty.
pub type OutputStreamBox = Pin<Box<dyn Stream<Item = Result<OutputChunk>> + Send>>;
pub type ByteStreamBox = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;
pub type StdinBox = Pin<Box<dyn AsyncWrite + Send>>;

/// One isolation engine. Implemented once per crate:
/// `vmlayer-firecracker`, `vmlayer-libkrun`, `vmlayer-docker`.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Stable identifier, also used in `Error::Unsupported` and `SnapshotRef`.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// The volume representation this backend mounts: directories for
    /// containers, block images for microVMs.
    fn volume_source_kind(&self) -> VolumeSourceKind;

    /// Is this backend usable on the current host? (KVM present, dockerd
    /// reachable, Hypervisor.framework entitlement granted...). Called once at
    /// daemon start to build the `Backend::Auto` preference list.
    async fn probe(&self) -> Result<()>;

    /// Boot a sandbox. Returns once the guest agent answers, so the handle is
    /// immediately usable for `exec`.
    async fn create(&self, spec: SandboxSpec) -> Result<Box<dyn SandboxHandle>>;

    /// Boot a sandbox from a snapshot. With `includes_memory` the guest
    /// resumes mid-execution; the backend must give it a fresh identity
    /// (new MAC/IP, new sandbox id) and re-establish the guest agent channel.
    async fn restore(
        &self,
        snapshot: &SnapshotRef,
        opts: RestoreOptions,
    ) -> Result<Box<dyn SandboxHandle>>;

    /// Reattach to a sandbox this backend already runs (daemon restart).
    async fn attach(&self, id: &SandboxId) -> Result<Box<dyn SandboxHandle>>;

    async fn list(&self) -> Result<Vec<SandboxInfo>>;

    async fn delete_snapshot(&self, snapshot: &SnapshotRef) -> Result<()>;

    /// Remove the contents of a host directory the engine wrote into (for
    /// example, root-owned files a container created in a volume) when the
    /// daemon itself lacks permission. Returns `Ok(false)` if this backend
    /// has no way to do that; the default.
    async fn cleanup_dir(&self, _path: &std::path::Path) -> Result<bool> {
        Ok(false)
    }
}

/// Turns an image reference into something a backend can boot.
///
/// Per ADR 0003 backends never see OCI references; resolution is its own
/// component. The Docker backend crate implements this itself (the engine
/// pulls); the daemon's OCI pipeline implements it for microVM backends.
#[async_trait]
pub trait ImageResolver: Send + Sync {
    async fn resolve(&self, image: &str) -> Result<ResolvedImage>;
}

/// One running (or paused) sandbox.
///
/// Methods guarded by a [`Capabilities`] field are marked *optional*; a backend
/// that lacks the capability returns [`crate::Error::Unsupported`] and must not
/// approximate.
#[async_trait]
pub trait SandboxHandle: Send + Sync {
    fn id(&self) -> &SandboxId;
    fn backend(&self) -> &'static str;

    async fn state(&self) -> Result<SandboxState>;

    // -- processes ----------------------------------------------------------

    /// Start a process. Returns as soon as the guest agent has spawned it.
    async fn exec(&self, req: ExecRequest) -> Result<Box<dyn Process>>;

    /// Open an interactive terminal. *Optional: `pty`.*
    async fn pty(&self, req: ExecRequest) -> Result<Box<dyn Pty>>;

    /// Processes started through vmlayer that are still running.
    async fn processes(&self) -> Result<Vec<ProcessInfo>>;

    /// Re-attach to a running process (e.g. after a client reconnect). The
    /// guest agent buffers output not yet consumed, up to a configured cap.
    async fn process(&self, pid: u32) -> Result<Box<dyn Process>>;

    /// Resolve when something inside the guest listens on `port`. Polled by
    /// the guest agent; cheap enough to call right after `create`.
    async fn wait_for_port(&self, port: u16, timeout: std::time::Duration) -> Result<()>;

    // -- files (all go through the guest agent, never the host mount) ---------

    async fn read_file(&self, path: &str) -> Result<Bytes>;
    async fn write_file(&self, path: &str, data: Bytes, mode: u32) -> Result<()>;
    async fn list_dir(&self, path: &str) -> Result<Vec<FileInfo>>;
    async fn remove(&self, path: &str, recursive: bool) -> Result<()>;
    async fn mkdir(&self, path: &str) -> Result<()>;

    // -- lifecycle ----------------------------------------------------------

    /// Persist the sandbox. Pauses for the duration, resumes unless
    /// `opts.stay_paused`. *Optional: `snapshot`.*
    async fn snapshot(&self, opts: SnapshotOptions) -> Result<SnapshotRef>;

    /// Clone `count` copies continuing from the current state. The parent
    /// keeps running. Each child gets a fresh identity and the cloned
    /// volumes in `mounts[i]` (one `Vec<Mount>` per child, prepared by the
    /// daemon). *Optional: `fork`.*
    ///
    /// On Firecracker this is snapshot plus `count` restores with memory
    /// mapped copy-on-write; it is the primitive agent runtimes branch on.
    async fn fork(
        &self,
        count: u32,
        mounts: Vec<Vec<Mount>>,
    ) -> Result<Vec<Box<dyn SandboxHandle>>>;

    /// Quiesce filesystems (`fsfreeze` through the guest agent) so the daemon
    /// can take consistent volume snapshots of a running sandbox, then thaw.
    /// Used by `Volume::snapshot` on an attached volume; `snapshot` above
    /// does this implicitly.
    async fn freeze_fs(&self) -> Result<()>;
    async fn thaw_fs(&self) -> Result<()>;

    /// *Optional: `pause`.*
    async fn pause(&self) -> Result<()>;
    async fn resume(&self) -> Result<()>;

    /// Destroy the sandbox and release all host resources. Idempotent.
    async fn kill(&self) -> Result<()>;

    // -- volumes -------------------------------------------------------------

    /// Attach a volume to a running sandbox. *Optional: `hot_attach`.*
    /// On Firecracker this needs PCI mode plus a bus rescan by the guest
    /// agent; no v1 backend implements it.
    async fn attach(&self, mount: Mount) -> Result<()>;
    async fn detach(&self, guest_path: &str) -> Result<()>;

    // -- network -------------------------------------------------------------

    /// Make a guest port reachable from the host. *Optional: `expose_port`.*
    async fn expose(&self, guest_port: u16) -> Result<Endpoint>;
    async fn unexpose(&self, guest_port: u16) -> Result<()>;
}

/// A process started with [`SandboxHandle::exec`].
#[async_trait]
pub trait Process: Send {
    fn pid(&self) -> u32;

    /// Interleaved stdout/stderr in arrival order. Take it once; a second
    /// call returns an empty stream.
    fn output(&mut self) -> OutputStreamBox;

    /// Writer for the process's stdin. Dropping it closes stdin.
    fn stdin(&mut self) -> Option<StdinBox>;

    async fn wait(&mut self) -> Result<ExitStatus>;

    async fn kill(&mut self, signal: i32) -> Result<()>;
}

/// An interactive terminal started with [`SandboxHandle::pty`].
#[async_trait]
pub trait Pty: Send {
    /// Raw terminal bytes, escapes intact.
    fn output(&mut self) -> ByteStreamBox;

    async fn write(&mut self, data: Bytes) -> Result<()>;

    async fn resize(&mut self, size: PtySize) -> Result<()>;

    async fn wait(&mut self) -> Result<ExitStatus>;

    async fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod object_safety {
    //! Compile-time proof that every trait can be boxed, which is how the
    //! daemon will hold them.
    use super::*;
    #[allow(dead_code)]
    fn assert_dyn(
        _b: Box<dyn Backend>,
        _s: Box<dyn SandboxHandle>,
        _p: Box<dyn Process>,
        _t: Box<dyn Pty>,
    ) {
    }
}
