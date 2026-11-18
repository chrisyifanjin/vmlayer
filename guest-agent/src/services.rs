//! The three gRPC services the agent serves. `Processes` and `Files` are the
//! same definitions the daemon serves; `sandbox_id` is ignored here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use vmlayer_core::{Error, ExecRequest, PtySize};
use vmlayer_procs::grpc::{event, exit_status, follow};
use vmlayer_procs::{Entry, Event, ProcessTable};
use vmlayer_proto::guest::v1 as guest;
use vmlayer_proto::guest::v1::guest_server::Guest;
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::files_server::Files;
use vmlayer_proto::v1::processes_server::Processes;

use crate::process::{LocalProcess, LocalPty};

type EventStream = Pin<Box<dyn Stream<Item = Result<pb::ExecEvent, Status>> + Send>>;
type PtyStream = Pin<Box<dyn Stream<Item = Result<pb::PtyOutput, Status>> + Send>>;

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

struct Inner {
    procs: ProcessTable,
    /// Real pid of the entrypoint; callers may address it as pid 1.
    entrypoint: AtomicU32,
    mounts: Mutex<Vec<PathBuf>>,
    started: Instant,
    /// Sandbox-wide environment and working directory (see `Configure`).
    defaults: std::sync::Mutex<(BTreeMap<String, String>, String)>,
}

impl Agent {
    pub fn new(output_buffer_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                procs: ProcessTable::new(output_buffer_bytes),
                entrypoint: AtomicU32::new(0),
                mounts: Mutex::new(Vec::new()),
                started: Instant::now(),
                defaults: std::sync::Mutex::new((BTreeMap::new(), String::new())),
            }),
        }
    }

    /// Layer a request over the sandbox defaults: its env entries win, and
    /// it runs in the default working directory unless it names one.
    fn with_defaults(&self, mut er: ExecRequest) -> ExecRequest {
        let defaults = self.inner.defaults.lock().unwrap();
        let mut env = defaults.0.clone();
        env.extend(std::mem::take(&mut er.env));
        er.env = env;
        if er.cwd.is_none() && !defaults.1.is_empty() {
            er.cwd = Some(defaults.1.clone());
        }
        er
    }

    fn resolve_pid(&self, pid: u32) -> u32 {
        if pid == 1 {
            let e = self.inner.entrypoint.load(Ordering::SeqCst);
            if e != 0 {
                return e;
            }
        }
        pid
    }

    async fn entry(&self, pid: u32) -> Result<Arc<Entry>, Status> {
        let pid = self.resolve_pid(pid);
        self.inner
            .procs
            .get(pid)
            .await
            .ok_or_else(|| Status::not_found(format!("no process {pid}")))
    }
}

fn status(e: Error) -> Status {
    match e {
        Error::Guest(m) => Status::failed_precondition(m),
        Error::Timeout(d) => Status::deadline_exceeded(format!("timed out after {d:?}")),
        Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Status::not_found(e.to_string())
        }
        other => Status::internal(other.to_string()),
    }
}

fn dur_from(d: Option<prost_types::Duration>) -> Option<Duration> {
    d.and_then(|d| Duration::try_from(d).ok())
        .filter(|d| !d.is_zero())
}

fn exec_request(
    argv: Vec<String>,
    cwd: String,
    env: std::collections::HashMap<String, String>,
    timeout: Option<prost_types::Duration>,
    pty: Option<PtySize>,
    user: String,
) -> Result<ExecRequest, Status> {
    if argv.is_empty() {
        return Err(Status::invalid_argument("argv is required"));
    }
    Ok(ExecRequest {
        argv,
        cwd: (!cwd.is_empty()).then_some(cwd),
        env: env.into_iter().collect::<BTreeMap<_, _>>(),
        timeout: dur_from(timeout),
        pty,
        user: (!user.is_empty()).then_some(user),
    })
}

#[tonic::async_trait]
impl Processes for Agent {
    type ExecStream = EventStream;
    type OutputStream = EventStream;
    type PtyStream = PtyStream;

    async fn exec(
        &self,
        req: Request<pb::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let r = req.into_inner();
        let er = self.with_defaults(exec_request(
            r.argv.clone(),
            r.cwd,
            r.env,
            r.timeout,
            None,
            r.user,
        )?);
        let timeout = er.timeout;
        let process = LocalProcess::spawn(&er).map_err(status)?;
        let entry = self
            .inner
            .procs
            .adopt(Box::new(process), r.argv, false, timeout)
            .await;
        if !r.stdin.is_empty() {
            entry
                .write_stdin(Bytes::from(r.stdin))
                .await
                .map_err(status)?;
        }
        if !r.detach {
            entry.close_stdin().await.map_err(status)?;
        }
        self.inner.procs.gc(64).await;
        let (tx, rx) = mpsc::channel(256);
        let _ = tx
            .send(Ok(pb::ExecEvent {
                event: Some(pb::exec_event::Event::Started(pb::Started {
                    pid: entry.pid,
                })),
            }))
            .await;
        if !r.detach {
            tokio::spawn(follow(entry, tx));
        }
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn output(
        &self,
        req: Request<pb::OutputRequest>,
    ) -> Result<Response<Self::OutputStream>, Status> {
        let entry = self.entry(req.into_inner().pid).await?;
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(follow(entry, tx));
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn stdin(
        &self,
        req: Request<Streaming<pb::StdinChunk>>,
    ) -> Result<Response<pb::Empty>, Status> {
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("empty stdin stream"))??;
        let entry = self.entry(first.pid).await?;
        if !first.data.is_empty() {
            entry
                .write_stdin(Bytes::from(first.data))
                .await
                .map_err(status)?;
        }
        while let Some(chunk) = inbound.next().await {
            let chunk = chunk?;
            if !chunk.data.is_empty() {
                entry
                    .write_stdin(Bytes::from(chunk.data))
                    .await
                    .map_err(status)?;
            }
        }
        entry.close_stdin().await.map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn wait(
        &self,
        req: Request<pb::WaitRequest>,
    ) -> Result<Response<pb::ExitStatus>, Status> {
        let r = req.into_inner();
        let entry = self.entry(r.pid).await?;
        let st = entry.wait(dur_from(r.timeout)).await.map_err(status)?;
        Ok(Response::new(exit_status(st)))
    }

    async fn kill(
        &self,
        req: Request<pb::KillProcessRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let entry = self.entry(r.pid).await?;
        entry
            .kill(if r.signal == 0 { 9 } else { r.signal })
            .await
            .map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn list(
        &self,
        _req: Request<pb::ListProcessesRequest>,
    ) -> Result<Response<pb::ListProcessesResponse>, Status> {
        let entry_pid = self.inner.entrypoint.load(Ordering::SeqCst);
        let processes = self
            .inner
            .procs
            .running()
            .await
            .into_iter()
            .map(|e| pb::ProcessInfo {
                // The entrypoint is reported as pid 1, matching the API contract.
                pid: if e.pid == entry_pid && entry_pid != 0 {
                    1
                } else {
                    e.pid
                },
                argv: e.argv.clone(),
                started_at: Some(prost_types::Timestamp::from(e.started_at)),
                is_entrypoint: e.is_entrypoint,
            })
            .collect();
        Ok(Response::new(pb::ListProcessesResponse { processes }))
    }

    async fn pty(
        &self,
        req: Request<Streaming<pb::PtyInput>>,
    ) -> Result<Response<Self::PtyStream>, Status> {
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("empty pty stream"))??;
        let open = match first.input {
            Some(pb::pty_input::Input::Open(o)) => o,
            _ => return Err(Status::invalid_argument("first pty message must be Open")),
        };
        let size = open
            .size
            .map(|s| PtySize {
                cols: s.cols as u16,
                rows: s.rows as u16,
            })
            .unwrap_or(PtySize { cols: 80, rows: 24 });
        let argv = if open.argv.is_empty() {
            vec!["/bin/sh".to_string()]
        } else {
            open.argv
        };
        let er = self.with_defaults(exec_request(
            argv,
            open.cwd,
            open.env,
            None,
            Some(size),
            open.user,
        )?);
        let mut pty = LocalPty::spawn(&er).map_err(status)?;
        let pid = pty.pid();
        let mut output = vmlayer_core::Pty::output(&mut pty);
        let pty = Arc::new(Mutex::new(pty));
        let (tx, rx) = mpsc::channel::<Result<pb::PtyOutput, Status>>(256);

        let out_tx = tx.clone();
        let out_pty = pty.clone();
        tokio::spawn(async move {
            let _ = out_tx
                .send(Ok(pb::PtyOutput {
                    output: Some(pb::pty_output::Output::Started(pb::Started { pid })),
                }))
                .await;
            while let Some(item) = output.next().await {
                match item {
                    Ok(b) => {
                        if out_tx
                            .send(Ok(pb::PtyOutput {
                                output: Some(pb::pty_output::Output::Data(b.to_vec())),
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = out_tx.send(Err(status(e))).await;
                        break;
                    }
                }
            }
            let st = vmlayer_core::Pty::wait(&mut *out_pty.lock().await)
                .await
                .unwrap_or(vmlayer_core::ExitStatus {
                    code: None,
                    signal: Some(9),
                    duration: Duration::ZERO,
                    timed_out: false,
                });
            let _ = out_tx
                .send(Ok(pb::PtyOutput {
                    output: Some(pb::pty_output::Output::Exited(exit_status(st))),
                }))
                .await;
        });

        let in_pty = pty.clone();
        tokio::spawn(async move {
            while let Some(msg) = inbound.next().await {
                let Ok(msg) = msg else { break };
                let res = match msg.input {
                    Some(pb::pty_input::Input::Data(d)) => {
                        vmlayer_core::Pty::write(&mut *in_pty.lock().await, Bytes::from(d)).await
                    }
                    Some(pb::pty_input::Input::Resize(s)) => {
                        vmlayer_core::Pty::resize(
                            &mut *in_pty.lock().await,
                            PtySize {
                                cols: s.cols as u16,
                                rows: s.rows as u16,
                            },
                        )
                        .await
                    }
                    _ => Ok(()),
                };
                if res.is_err() {
                    break;
                }
            }
            let _ = vmlayer_core::Pty::close(&mut *in_pty.lock().await).await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

fn file_info(path: &Path, meta: &std::fs::Metadata) -> pb::FileInfo {
    use std::os::unix::fs::PermissionsExt;
    pb::FileInfo {
        path: path.display().to_string(),
        size: meta.len(),
        is_dir: meta.is_dir(),
        mode: meta.permissions().mode() & 0o7777,
        modified_at: meta.modified().ok().map(prost_types::Timestamp::from),
    }
}

fn io_status(path: &str, e: std::io::Error) -> Status {
    match e.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(format!("{path}: not found")),
        std::io::ErrorKind::PermissionDenied => {
            Status::permission_denied(format!("{path}: permission denied"))
        }
        _ => Status::failed_precondition(format!("{path}: {e}")),
    }
}

#[tonic::async_trait]
impl Files for Agent {
    type ReadStream = ReceiverStream<Result<pb::FileChunk, Status>>;

    async fn read(
        &self,
        req: Request<pb::ReadFileRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let path = req.into_inner().path;
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| io_status(&path, e))?;
        if file.metadata().await.map(|m| m.is_dir()).unwrap_or(false) {
            return Err(Status::failed_precondition(format!(
                "{path}: is a directory"
            )));
        }
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx
                            .send(Ok(pb::FileChunk {
                                data: buf[..n].to_vec(),
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                        break;
                    }
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn write(
        &self,
        req: Request<Streaming<pb::WriteFileRequest>>,
    ) -> Result<Response<pb::Empty>, Status> {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::AsyncWriteExt;
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("empty write stream"))??;
        let header = match first.part {
            Some(pb::write_file_request::Part::Header(h)) => h,
            _ => return Err(Status::invalid_argument("first message must be a header")),
        };
        let path = PathBuf::from(&header.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_status(&header.path, e))?;
        }
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| io_status(&header.path, e))?;
        while let Some(msg) = inbound.next().await {
            if let Some(pb::write_file_request::Part::Data(d)) = msg?.part {
                file.write_all(&d)
                    .await
                    .map_err(|e| io_status(&header.path, e))?;
            }
        }
        file.flush().await.map_err(|e| io_status(&header.path, e))?;
        let mode = if header.mode == 0 { 0o644 } else { header.mode };
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(|e| io_status(&header.path, e))?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn stat(&self, req: Request<pb::StatRequest>) -> Result<Response<pb::FileInfo>, Status> {
        let path = req.into_inner().path;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| io_status(&path, e))?;
        Ok(Response::new(file_info(Path::new(&path), &meta)))
    }

    async fn list(
        &self,
        req: Request<pb::ListFilesRequest>,
    ) -> Result<Response<pb::ListFilesResponse>, Status> {
        let path = req.into_inner().path;
        let mut rd = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| io_status(&path, e))?;
        let mut entries = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(|e| io_status(&path, e))? {
            if let Ok(meta) = entry.metadata().await {
                entries.push(file_info(&entry.path(), &meta));
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Response::new(pb::ListFilesResponse { entries }))
    }

    async fn mkdir(&self, req: Request<pb::MkdirRequest>) -> Result<Response<pb::Empty>, Status> {
        let path = req.into_inner().path;
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| io_status(&path, e))?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn remove(&self, req: Request<pb::RemoveRequest>) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let meta = match tokio::fs::symlink_metadata(&r.path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Response::new(pb::Empty {}))
            }
            Err(e) => return Err(io_status(&r.path, e)),
        };
        let res = if meta.is_dir() {
            if r.recursive {
                tokio::fs::remove_dir_all(&r.path).await
            } else {
                tokio::fs::remove_dir(&r.path).await
            }
        } else {
            tokio::fs::remove_file(&r.path).await
        };
        res.map_err(|e| io_status(&r.path, e))?;
        Ok(Response::new(pb::Empty {}))
    }
}

#[tonic::async_trait]
impl Guest for Agent {
    async fn ready(
        &self,
        _req: Request<pb::Empty>,
    ) -> Result<Response<guest::ReadyResponse>, Status> {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        Ok(Response::new(guest::ReadyResponse {
            agent_version: crate::VERSION.to_string(),
            kernel: kernel.trim().to_string(),
            uptime: self.inner.started.elapsed().as_secs_f64(),
        }))
    }

    async fn configure(
        &self,
        req: Request<guest::ConfigureRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        if !r.hostname.is_empty() {
            nix::unistd::sethostname(&r.hostname)
                .map_err(|e| Status::failed_precondition(format!("hostname: {e}")))?;
        }
        if !r.hosts.is_empty() {
            let current = tokio::fs::read_to_string("/etc/hosts")
                .await
                .unwrap_or_default();
            let updated = crate::fs::with_hosts(&current, &r.hosts);
            tokio::fs::write("/etc/hosts", updated)
                .await
                .map_err(|e| Status::failed_precondition(format!("/etc/hosts: {e}")))?;
        }
        if !r.cwd.is_empty() {
            tokio::fs::create_dir_all(&r.cwd)
                .await
                .map_err(|e| Status::failed_precondition(format!("{}: {e}", r.cwd)))?;
        }
        *self.inner.defaults.lock().unwrap() = (r.env.into_iter().collect(), r.cwd);
        Ok(Response::new(pb::Empty {}))
    }

    async fn mount(
        &self,
        req: Request<guest::MountRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let path = r.path.clone();
        tokio::task::spawn_blocking(move || {
            crate::fs::mount(&r.source, &r.path, &r.fstype, r.read_only)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(status)?;
        self.inner.mounts.lock().await.push(PathBuf::from(path));
        Ok(Response::new(pb::Empty {}))
    }

    async fn unmount(
        &self,
        req: Request<guest::UnmountRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let path = req.into_inner().path;
        let p = path.clone();
        tokio::task::spawn_blocking(move || crate::fs::unmount(&p))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(status)?;
        self.inner
            .mounts
            .lock()
            .await
            .retain(|m| m != Path::new(&path));
        Ok(Response::new(pb::Empty {}))
    }

    async fn start_entrypoint(
        &self,
        req: Request<guest::EntrypointRequest>,
    ) -> Result<Response<pb::Started>, Status> {
        let r = req.into_inner();
        let er = self.with_defaults(exec_request(
            r.argv.clone(),
            r.cwd,
            r.env,
            None,
            None,
            String::new(),
        )?);
        let process = LocalProcess::spawn(&er).map_err(status)?;
        let entry = self
            .inner
            .procs
            .adopt(Box::new(process), r.argv, true, None)
            .await;
        entry.close_stdin().await.map_err(status)?;
        self.inner.entrypoint.store(entry.pid, Ordering::SeqCst);
        Ok(Response::new(pb::Started { pid: 1 }))
    }

    async fn wait_for_port(
        &self,
        req: Request<guest::WaitForPortRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let r = req.into_inner();
        let port = u16::try_from(r.port).map_err(|_| Status::invalid_argument("bad port"))?;
        let timeout = dur_from(r.timeout).unwrap_or(Duration::from_secs(30));
        let deadline = Instant::now() + timeout;
        loop {
            let tables = [
                std::fs::read_to_string("/proc/net/tcp").unwrap_or_default(),
                std::fs::read_to_string("/proc/net/tcp6").unwrap_or_default(),
            ];
            if tables.iter().any(|t| crate::netstat::listening_on(t, port)) {
                return Ok(Response::new(pb::Empty {}));
            }
            if Instant::now() >= deadline {
                return Err(Status::deadline_exceeded(format!(
                    "nothing listening on {port} after {timeout:?}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn freeze_fs(&self, _req: Request<pb::Empty>) -> Result<Response<pb::Empty>, Status> {
        for m in self.inner.mounts.lock().await.iter() {
            crate::fs::freeze(m).map_err(status)?;
        }
        Ok(Response::new(pb::Empty {}))
    }

    async fn thaw_fs(&self, _req: Request<pb::Empty>) -> Result<Response<pb::Empty>, Status> {
        for m in self.inner.mounts.lock().await.iter() {
            crate::fs::thaw(m).map_err(status)?;
        }
        Ok(Response::new(pb::Empty {}))
    }

    async fn rescan_devices(
        &self,
        _req: Request<pb::Empty>,
    ) -> Result<Response<pb::Empty>, Status> {
        crate::fs::rescan_pci().map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn shutdown(&self, _req: Request<pb::Empty>) -> Result<Response<pb::Empty>, Status> {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            unsafe { libc::sync() };
            let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
            std::process::exit(0);
        });
        Ok(Response::new(pb::Empty {}))
    }
}

// Silence an unused-import warning when Event is only used by `follow`.
#[allow(dead_code)]
fn _event(e: &Event) -> pb::ExecEvent {
    event(e)
}
