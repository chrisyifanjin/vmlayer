//! `Process` and `Pty` for local children of the agent.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use vmlayer_core::backend::{ByteStreamBox, OutputStreamBox, StdinBox};
use vmlayer_core::{
    Error, ExecRequest, ExitStatus, OutputChunk, OutputStream, Process, Pty, PtySize, Result,
};

fn status_of(st: std::process::ExitStatus, started: Instant) -> ExitStatus {
    ExitStatus {
        code: st.code(),
        signal: st.signal(),
        duration: started.elapsed(),
        timed_out: false,
    }
}

fn signal_from(n: i32) -> Signal {
    Signal::try_from(n).unwrap_or(Signal::SIGKILL)
}

/// The account a command runs as.
#[derive(Debug, PartialEq, Eq)]
struct Account {
    uid: u32,
    gid: u32,
    name: Option<String>,
    home: Option<String>,
}

/// Resolve `user` (a name, a UID, or `uid:gid`) against a passwd file.
fn resolve_user(user: &str, passwd: &str) -> Result<Account> {
    let entries = passwd.lines().filter_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 7).then(|| {
            (
                f[0],
                f[2].parse::<u32>().ok(),
                f[3].parse::<u32>().ok(),
                f[5],
            )
        })
    });
    let account = |name: &str, uid: u32, gid: u32, home: &str| Account {
        uid,
        gid,
        name: Some(name.to_string()),
        home: (!home.is_empty()).then(|| home.to_string()),
    };
    if let Some((uid, gid)) = user.split_once(':') {
        let parse = |s: &str| {
            s.parse::<u32>()
                .map_err(|_| Error::Guest(format!("user {user:?}: expected uid:gid")))
        };
        let (uid, gid) = (parse(uid)?, parse(gid)?);
        let known = entries.into_iter().find(|e| e.1 == Some(uid));
        return Ok(Account {
            uid,
            gid,
            name: known.map(|e| e.0.to_string()),
            home: known.map(|e| e.3.to_string()).filter(|h| !h.is_empty()),
        });
    }
    let by_uid = user.parse::<u32>().ok();
    for (name, uid, gid, home) in entries {
        let (Some(uid), Some(gid)) = (uid, gid) else {
            continue;
        };
        if name == user || by_uid == Some(uid) {
            return Ok(account(name, uid, gid, home));
        }
    }
    match by_uid {
        // A UID with no passwd entry runs with the same GID, like `docker exec -u`.
        Some(uid) => Ok(Account {
            uid,
            gid: uid,
            name: None,
            home: None,
        }),
        None => Err(Error::Guest(format!("no such user in /etc/passwd: {user}"))),
    }
}

fn command(req: &ExecRequest) -> Result<Command> {
    let mut cmd = Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..]);
    if let Some(user) = req.user.as_deref().filter(|u| !u.is_empty()) {
        let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        let account = resolve_user(user, &passwd)?;
        // Like `su -`: the user's own HOME and USER unless the caller set them.
        // `/` when the passwd file has no home for it, as `docker exec -u` does.
        cmd.env("HOME", account.home.as_deref().unwrap_or("/"));
        if let Some(name) = &account.name {
            cmd.env("USER", name).env("LOGNAME", name);
        }
        // Drops supplementary groups too (std calls setgroups when root).
        cmd.uid(account.uid).gid(account.gid);
    }
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    cmd.kill_on_drop(false);
    Ok(cmd)
}

pub struct LocalProcess {
    child: Arc<Mutex<Child>>,
    pid: u32,
    output: Option<OutputStreamBox>,
    stdin: Option<StdinBox>,
    started: Instant,
}

impl LocalProcess {
    pub fn spawn(req: &ExecRequest) -> Result<Self> {
        let mut cmd = command(req)?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so `kill` can take the whole tree.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Guest(format!("spawn {:?}: {e}", req.argv[0])))?;
        let pid = child
            .id()
            .ok_or_else(|| Error::Guest("child has no pid".into()))?;
        let stdin = child.stdin.take().map(|s| Box::pin(s) as StdinBox);
        let (tx, rx) = mpsc::channel::<Result<OutputChunk>>(256);
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, OutputStream::Stdout, tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, OutputStream::Stderr, tx));
        }
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            pid,
            output: Some(Box::pin(ReceiverStream::new(rx))),
            stdin,
            started: Instant::now(),
        })
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    stream: OutputStream,
    tx: mpsc::Sender<Result<OutputChunk>>,
) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let chunk = OutputChunk {
                    stream,
                    data: Bytes::copy_from_slice(&buf[..n]),
                };
                if tx.send(Ok(chunk)).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(Error::Io(e))).await;
                break;
            }
        }
    }
}

#[async_trait]
impl Process for LocalProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn output(&mut self) -> OutputStreamBox {
        self.output
            .take()
            .unwrap_or_else(|| Box::pin(futures::stream::empty()))
    }

    fn stdin(&mut self) -> Option<StdinBox> {
        self.stdin.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        let st = self.child.lock().await.wait().await?;
        Ok(status_of(st, self.started))
    }

    async fn kill(&mut self, signal: i32) -> Result<()> {
        // Whole process group first, then the leader in case it left the group.
        let _ = kill(Pid::from_raw(-(self.pid as i32)), signal_from(signal));
        match kill(Pid::from_raw(self.pid as i32), signal_from(signal)) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(e) => Err(Error::Guest(format!("kill {}: {e}", self.pid))),
        }
    }
}

pub struct LocalPty {
    child: Arc<Mutex<Child>>,
    pid: u32,
    master: Arc<AsyncFd<OwnedFd>>,
    output: Option<ByteStreamBox>,
    started: Instant,
}

impl LocalPty {
    pub fn spawn(req: &ExecRequest) -> Result<Self> {
        let size = req.pty.unwrap_or(PtySize { cols: 80, rows: 24 });
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&ws), None)
            .map_err(|e| Error::Guest(format!("openpty: {e}")))?;
        let slave_fd = pty.slave.as_raw_fd();
        let mut cmd = command(req)?;
        cmd.env(
            "TERM",
            req.env
                .get("TERM")
                .cloned()
                .unwrap_or_else(|| "xterm-256color".into()),
        );
        let slave_in = pty.slave.try_clone()?;
        let slave_out = pty.slave.try_clone()?;
        let slave_err = pty.slave.try_clone()?;
        cmd.stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err));
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| Error::Guest(format!("spawn {:?}: {e}", req.argv[0])))?;
        drop(pty.slave);
        let pid = child
            .id()
            .ok_or_else(|| Error::Guest("child has no pid".into()))?;
        set_nonblocking(pty.master.as_raw_fd())?;
        // SAFETY: `pty.master` is an OwnedFd we exclusively own; it stays
        // open for the AsyncFd's lifetime.
        let master =
            Arc::new(unsafe { AsyncFd::register(pty.master) }.map_err(|e| Error::Io(e.into()))?);
        let (tx, rx) = mpsc::channel::<Result<Bytes>>(256);
        let reader = master.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                let mut guard = match reader.readable().await {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::read(inner.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        if tx
                            .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // EIO: the slave side closed, i.e. the child exited.
                    Ok(Err(_)) => break,
                    Err(_would_block) => continue,
                }
            }
        });
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            pid,
            master,
            output: Some(Box::pin(ReceiverStream::new(rx))),
            started: Instant::now(),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

fn set_nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[async_trait]
impl Pty for LocalPty {
    fn output(&mut self) -> ByteStreamBox {
        self.output
            .take()
            .unwrap_or_else(|| Box::pin(futures::stream::empty()))
    }

    async fn write(&mut self, data: Bytes) -> Result<()> {
        let mut rest = &data[..];
        while !rest.is_empty() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|inner| {
                let n = unsafe {
                    libc::write(inner.as_raw_fd(), rest.as_ptr() as *const _, rest.len())
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => rest = &rest[n..],
                Ok(Err(e)) => return Err(Error::Io(e)),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    async fn resize(&mut self, size: PtySize) -> Result<()> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        if rc == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        let st = self.child.lock().await.wait().await?;
        Ok(status_of(st, self.started))
    }

    async fn close(&mut self) -> Result<()> {
        let _ = kill(Pid::from_raw(self.pid as i32), Signal::SIGHUP);
        let _ = kill(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
        Ok(())
    }
}

// Keep the unused-import linter honest on non-pty paths.
#[allow(dead_code)]
async fn _touch(mut s: StdinBox) {
    let _ = s.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          agent:x:1000:1001:Agent:/home/agent:/bin/bash\n\
                          broken:x:abc:0::/:/bin/sh\n";

    #[test]
    fn users_resolve_by_name_uid_and_pair() {
        let agent = resolve_user("agent", PASSWD).unwrap();
        assert_eq!((agent.uid, agent.gid), (1000, 1001));
        assert_eq!(agent.home.as_deref(), Some("/home/agent"));
        assert_eq!(resolve_user("1000", PASSWD).unwrap(), agent);
        let pair = resolve_user("1000:5", PASSWD).unwrap();
        assert_eq!(
            (pair.uid, pair.gid, pair.name.as_deref()),
            (1000, 5, Some("agent"))
        );
        let unknown = resolve_user("4242", PASSWD).unwrap();
        assert_eq!((unknown.uid, unknown.gid, unknown.name), (4242, 4242, None));
        assert!(resolve_user("nobody-here", PASSWD).is_err());
        assert!(resolve_user("broken", PASSWD).is_err());
        assert!(resolve_user("1:x", PASSWD).is_err());
    }
}
