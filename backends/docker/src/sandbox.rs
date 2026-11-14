//! The per-sandbox handle: one container plus its process registry.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::query_parameters::{CommitContainerOptionsBuilder, RemoveContainerOptionsBuilder};
use bollard::Docker;
use bytes::Bytes;
use tokio::sync::Mutex;
use uuid::Uuid;
use vmlayer_core::{
    Endpoint, Error, ExecRequest, FileInfo, Mount, Process, ProcessInfo, Pty, Result,
    SandboxHandle, SandboxId, SandboxSpec, SandboxState, SnapshotId, SnapshotOptions, SnapshotRef,
};

use crate::process::{DockerProcess, DockerPty};
use crate::{engine_err, state_from_status, Config, BACKEND_NAME};

/// Per-sandbox scratch directory inside the container for pid and log files.
pub(crate) const STATE_DIR: &str = "/tmp/.vmlayer";

pub(crate) struct ProcEntry {
    pub exec_id: String,
    pub argv: Vec<String>,
    pub started_at: SystemTime,
    pub is_entrypoint: bool,
}

pub struct DockerSandbox {
    pub(crate) docker: Arc<Docker>,
    pub(crate) config: Config,
    pub(crate) id: SandboxId,
    pub(crate) container_id: String,
    pub(crate) procs: Mutex<HashMap<u32, ProcEntry>>,
    next_pid: AtomicU32,
    /// guest port -> helper container id
    pub(crate) exposed: Mutex<HashMap<u16, (String, Endpoint)>>,
    /// Spec recorded for snapshots. `None` for attached handles.
    pub(crate) spec: Mutex<Option<SandboxSpec>>,
}

impl DockerSandbox {
    pub(crate) fn new(
        docker: Arc<Docker>,
        config: Config,
        id: SandboxId,
        container_id: String,
    ) -> Self {
        Self {
            docker,
            config,
            id,
            container_id,
            procs: Mutex::new(HashMap::new()),
            next_pid: AtomicU32::new(2),
            exposed: Mutex::new(HashMap::new()),
            spec: Mutex::new(None),
        }
    }

    pub(crate) async fn remember_spec(&self, spec: SandboxSpec) {
        *self.spec.lock().await = Some(spec);
    }

    pub(crate) async fn start_entrypoint(&self, req: ExecRequest) -> Result<()> {
        // Detached, like `docker exec -d`: an attached exec dies with its
        // client connection. Output goes to a log the daemon can read back.
        let exec_id = self.create_and_start_detached(&req, 1).await?;
        self.procs.lock().await.insert(
            1,
            ProcEntry {
                exec_id,
                argv: req.argv,
                started_at: SystemTime::now(),
                is_entrypoint: true,
            },
        );
        Ok(())
    }

    /// Wrap an argv so the in-container pid is recorded under our pid token.
    /// Docker's exec inspect reports the host-namespace pid, which is useless
    /// inside the container, so `kill` reads this file instead.
    fn wrap_argv(argv: &[String], pid: u32, log: Option<&str>) -> Vec<String> {
        let redirect = log.map(|l| format!(" >{l} 2>&1")).unwrap_or_default();
        let script = format!(
            "mkdir -p {d} && echo $$ > {d}/$0.pid && exec \"$@\"{redirect}",
            d = STATE_DIR
        );
        let mut v = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script,
            pid.to_string(),
        ];
        v.extend(argv.iter().cloned());
        v
    }

    fn exec_options(
        &self,
        req: &ExecRequest,
        cmd: Vec<String>,
        tty: bool,
        attach_stdin: bool,
        attach_out: bool,
    ) -> CreateExecOptions<String> {
        let env: Vec<String> = req.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        CreateExecOptions {
            attach_stdin: Some(attach_stdin),
            attach_stdout: Some(attach_out),
            attach_stderr: Some(attach_out),
            tty: Some(tty),
            env: if env.is_empty() { None } else { Some(env) },
            cmd: Some(cmd),
            working_dir: req.cwd.clone(),
            ..Default::default()
        }
    }

    /// Start an exec with no client connection; output goes to a log file.
    async fn create_and_start_detached(&self, req: &ExecRequest, pid: u32) -> Result<String> {
        let log = format!("{STATE_DIR}/{pid}.log");
        let cmd = Self::wrap_argv(&req.argv, pid, Some(&log));
        let opts = self.exec_options(req, cmd, false, false, false);
        let created = self
            .docker
            .create_exec(&self.container_id, opts)
            .await
            .map_err(engine_err)?;
        self.docker
            .start_exec(
                &created.id,
                Some(StartExecOptions {
                    detach: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(engine_err)?;
        Ok(created.id)
    }

    /// Create and start an attached exec. Returns the exec id and the streams.
    pub(crate) async fn create_and_start(
        &self,
        req: &ExecRequest,
        pid: u32,
        tty: bool,
        attach_stdin: bool,
    ) -> Result<(String, StartExecResults)> {
        let cmd = Self::wrap_argv(&req.argv, pid, None);
        let opts = self.exec_options(req, cmd, tty, attach_stdin, true);
        let created = self
            .docker
            .create_exec(&self.container_id, opts)
            .await
            .map_err(engine_err)?;
        let started = self
            .docker
            .start_exec(
                &created.id,
                Some(StartExecOptions {
                    detach: false,
                    tty,
                    ..Default::default()
                }),
            )
            .await
            .map_err(engine_err)?;
        Ok((created.id, started))
    }

    /// Run a command to completion and capture its output. Used for the
    /// file metadata helpers; not exposed to callers.
    pub(crate) async fn run_capture(&self, argv: Vec<String>) -> Result<(i32, String, String)> {
        let req = ExecRequest {
            argv,
            cwd: None,
            env: BTreeMap::new(),
            timeout: None,
            pty: None,
        };
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        let (exec_id, started) = self.create_and_start(&req, pid, false, false).await?;
        let mut proc = DockerProcess::new(
            self.docker.clone(),
            self.container_id.clone(),
            exec_id,
            pid,
            started,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            use futures::StreamExt;
            let mut stream = proc.output();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                match chunk.stream {
                    vmlayer_core::OutputStream::Stdout => out.extend_from_slice(&chunk.data),
                    vmlayer_core::OutputStream::Stderr => err.extend_from_slice(&chunk.data),
                }
            }
        }
        let status = proc.wait().await?;
        Ok((
            status.code.unwrap_or(-1),
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ))
    }

    async fn inspect_state(&self) -> Result<SandboxState> {
        let info = self
            .docker
            .inspect_container(&self.container_id, None)
            .await
            .map_err(|e| match e {
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                } => Error::SandboxNotFound(self.id.clone()),
                e => engine_err(e),
            })?;
        let status = info
            .state
            .and_then(|s| s.status)
            .map(|s| format!("{s:?}").to_lowercase());
        Ok(state_from_status(status.as_deref()))
    }
}

#[async_trait]
impl SandboxHandle for DockerSandbox {
    fn id(&self) -> &SandboxId {
        &self.id
    }

    fn backend(&self) -> &'static str {
        BACKEND_NAME
    }

    async fn state(&self) -> Result<SandboxState> {
        self.inspect_state().await
    }

    async fn exec(&self, req: ExecRequest) -> Result<Box<dyn Process>> {
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        let (exec_id, started) = self.create_and_start(&req, pid, false, true).await?;
        self.procs.lock().await.insert(
            pid,
            ProcEntry {
                exec_id: exec_id.clone(),
                argv: req.argv.clone(),
                started_at: SystemTime::now(),
                is_entrypoint: false,
            },
        );
        Ok(Box::new(DockerProcess::new(
            self.docker.clone(),
            self.container_id.clone(),
            exec_id,
            pid,
            started,
        )))
    }

    async fn pty(&self, req: ExecRequest) -> Result<Box<dyn Pty>> {
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        let (exec_id, started) = self.create_and_start(&req, pid, true, true).await?;
        if let Some(size) = req.pty {
            let opts = bollard::query_parameters::ResizeExecOptionsBuilder::new()
                .h(i32::from(size.rows))
                .w(i32::from(size.cols))
                .build();
            // Best effort: the exec may not have a tty attached yet.
            let _ = self.docker.resize_exec(&exec_id, opts).await;
        }
        self.procs.lock().await.insert(
            pid,
            ProcEntry {
                exec_id: exec_id.clone(),
                argv: req.argv.clone(),
                started_at: SystemTime::now(),
                is_entrypoint: false,
            },
        );
        Ok(Box::new(DockerPty::new(
            self.docker.clone(),
            self.container_id.clone(),
            exec_id,
            pid,
            started,
        )))
    }

    async fn processes(&self) -> Result<Vec<ProcessInfo>> {
        let procs = self.procs.lock().await;
        let mut out = Vec::new();
        for (pid, p) in procs.iter() {
            let running = self
                .docker
                .inspect_exec(&p.exec_id)
                .await
                .ok()
                .and_then(|i| i.running)
                .unwrap_or(false);
            if running {
                out.push(ProcessInfo {
                    pid: *pid,
                    argv: p.argv.clone(),
                    started_at: p.started_at,
                    is_entrypoint: p.is_entrypoint,
                });
            }
        }
        out.sort_by_key(|p| p.pid);
        Ok(out)
    }

    async fn process(&self, pid: u32) -> Result<Box<dyn Process>> {
        let procs = self.procs.lock().await;
        let entry = procs
            .get(&pid)
            .ok_or_else(|| Error::Guest(format!("no process {pid}")))?;
        Ok(Box::new(DockerProcess::detached(
            self.docker.clone(),
            self.container_id.clone(),
            entry.exec_id.clone(),
            pid,
        )))
    }

    async fn wait_for_port(&self, port: u16, timeout: Duration) -> Result<()> {
        let hex = format!("{port:04X}");
        let tries = (timeout.as_millis() / 200).max(1);
        let script = format!(
            "i=0; while [ $i -lt {tries} ]; do \
               if grep -qiE ':{hex} [0-9A-F]+:[0-9A-F]+ 0A' /proc/net/tcp /proc/net/tcp6 2>/dev/null; then exit 0; fi; \
               i=$((i+1)); sleep 0.2; done; exit 1"
        );
        let (code, _, _) = self
            .run_capture(vec!["/bin/sh".into(), "-c".into(), script])
            .await?;
        if code == 0 {
            Ok(())
        } else {
            Err(Error::Timeout(timeout))
        }
    }

    async fn read_file(&self, path: &str) -> Result<Bytes> {
        crate::files::read_file(self, path).await
    }

    async fn write_file(&self, path: &str, data: Bytes, mode: u32) -> Result<()> {
        crate::files::write_file(self, path, data, mode).await
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<FileInfo>> {
        crate::files::list_dir(self, path).await
    }

    async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        crate::files::remove(self, path, recursive).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        crate::files::mkdir(self, path).await
    }

    async fn snapshot(&self, opts: SnapshotOptions) -> Result<SnapshotRef> {
        let spec = self.spec.lock().await.clone().ok_or_else(|| {
            Error::Guest("snapshot of an attached handle is not supported".into())
        })?;
        let was_paused = matches!(self.inspect_state().await?, SandboxState::Paused);
        if !was_paused {
            self.docker
                .pause_container(&self.container_id)
                .await
                .map_err(engine_err)?;
        }
        let id = SnapshotId(format!("snap-{}", Uuid::now_v7().simple()));
        let commit = CommitContainerOptionsBuilder::new()
            .container(&self.container_id)
            .repo("vmlayer/snapshot")
            .tag(&id.0)
            .pause(false)
            .build();
        let result = self
            .docker
            .commit_container(commit, bollard::models::ContainerConfig::default())
            .await;
        if !was_paused && !opts.stay_paused {
            self.docker
                .unpause_container(&self.container_id)
                .await
                .map_err(engine_err)?;
        }
        let image = result.map_err(engine_err)?;
        let size = self
            .docker
            .inspect_image(&image.id)
            .await
            .ok()
            .and_then(|i| i.size)
            .unwrap_or(0)
            .max(0) as u64;
        Ok(SnapshotRef {
            id,
            backend: BACKEND_NAME,
            engine_ref: image.id,
            spec,
            engine_version: String::new(), // filled by the daemon from SystemInfo
            cpu_model: None,
            created_at: SystemTime::now(),
            size_bytes: size,
            includes_memory: false,
            volumes: Vec::new(),
            labels: opts.labels,
        })
    }

    async fn fork(
        &self,
        _count: u32,
        _mounts: Vec<Vec<Mount>>,
    ) -> Result<Vec<Box<dyn SandboxHandle>>> {
        Err(Error::Unsupported {
            backend: BACKEND_NAME,
            feature: "fork",
        })
    }

    async fn freeze_fs(&self) -> Result<()> {
        let _ = self.run_capture(vec!["sync".into()]).await;
        Ok(())
    }

    async fn thaw_fs(&self) -> Result<()> {
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.docker
            .pause_container(&self.container_id)
            .await
            .map_err(engine_err)
    }

    async fn resume(&self) -> Result<()> {
        self.docker
            .unpause_container(&self.container_id)
            .await
            .map_err(engine_err)
    }

    async fn kill(&self) -> Result<()> {
        let helpers: Vec<String> = self
            .exposed
            .lock()
            .await
            .drain()
            .map(|(_, (cid, _))| cid)
            .collect();
        for cid in helpers {
            let _ = self
                .docker
                .remove_container(
                    &cid,
                    Some(
                        RemoveContainerOptionsBuilder::new()
                            .force(true)
                            .v(true)
                            .build(),
                    ),
                )
                .await;
        }
        match self
            .docker
            .remove_container(
                &self.container_id,
                Some(
                    RemoveContainerOptionsBuilder::new()
                        .force(true)
                        .v(true)
                        .build(),
                ),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(engine_err(e)),
        }
    }

    async fn attach(&self, _mount: Mount) -> Result<()> {
        Err(Error::Unsupported {
            backend: BACKEND_NAME,
            feature: "hot attach",
        })
    }

    async fn detach(&self, _guest_path: &str) -> Result<()> {
        Err(Error::Unsupported {
            backend: BACKEND_NAME,
            feature: "hot detach",
        })
    }

    async fn expose(&self, guest_port: u16) -> Result<Endpoint> {
        crate::expose::expose(self, guest_port).await
    }

    async fn unexpose(&self, guest_port: u16) -> Result<()> {
        crate::expose::unexpose(self, guest_port).await
    }
}
