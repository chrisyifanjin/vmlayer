//! Commands, processes, output and file metadata.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

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
    /// Run as this user: a name in the guest's `/etc/passwd`, a UID, or
    /// `uid:gid`. `None` runs as the image's default user.
    #[serde(default)]
    pub user: Option<String>,
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

/// A process the guest agent is tracking.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub argv: Vec<String>,
    pub started_at: SystemTime,
    pub is_entrypoint: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Exit code, or `None` if killed by a signal.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub duration: Duration,
    /// Set by the daemon when it killed the process for exceeding its
    /// `ExecRequest::timeout`. Backends leave it `false`.
    pub timed_out: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub mode: u32,
    pub modified_at: SystemTime,
}
