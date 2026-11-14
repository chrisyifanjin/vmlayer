//! `Process` and `Pty` over Docker execs.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::Docker;
use bytes::Bytes;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use vmlayer_core::backend::{ByteStreamBox, OutputStreamBox, StdinBox};
use vmlayer_core::{Error, ExitStatus, OutputChunk, OutputStream, Process, Pty, PtySize, Result};

use crate::engine_err;

type RawOutput = Pin<
    Box<dyn futures::Stream<Item = std::result::Result<LogOutput, bollard::errors::Error>> + Send>,
>;

pub struct DockerProcess {
    docker: Arc<Docker>,
    container_id: String,
    exec_id: String,
    pid: u32,
    output: Option<RawOutput>,
    input: Option<StdinBox>,
    started: Instant,
}

impl DockerProcess {
    pub(crate) fn new(
        docker: Arc<Docker>,
        container_id: String,
        exec_id: String,
        pid: u32,
        started: StartExecResults,
    ) -> Self {
        let (output, input) = match started {
            StartExecResults::Attached { output, input } => (Some(output), Some(input)),
            StartExecResults::Detached => (None, None),
        };
        Self {
            docker,
            container_id,
            exec_id,
            pid,
            output,
            input,
            started: Instant::now(),
        }
    }

    /// A handle with no output stream, for re-attaching by pid.
    pub(crate) fn detached(
        docker: Arc<Docker>,
        container_id: String,
        exec_id: String,
        pid: u32,
    ) -> Self {
        Self {
            docker,
            container_id,
            exec_id,
            pid,
            output: None,
            input: None,
            started: Instant::now(),
        }
    }
}

pub(crate) async fn wait_exec(
    docker: &Docker,
    exec_id: &str,
    started: Instant,
) -> Result<ExitStatus> {
    let mut delay = Duration::from_millis(20);
    loop {
        let info = docker.inspect_exec(exec_id).await.map_err(engine_err)?;
        if info.running == Some(false) {
            let code = info.exit_code.map(|c| c as i32);
            // Docker reports signal deaths as 128+n.
            let (code, signal) = match code {
                Some(c) if c > 128 && c < 160 => (None, Some(c - 128)),
                other => (other, None),
            };
            return Ok(ExitStatus {
                code,
                signal,
                duration: started.elapsed(),
                timed_out: false,
            });
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

/// Signal a process by our pid token: reads the in-container pid the exec
/// wrapper recorded (see `DockerSandbox::wrap_argv`). Docker's own exec
/// inspect reports the host-namespace pid, which means nothing in here.
pub(crate) async fn kill_exec(
    docker: &Docker,
    container_id: &str,
    exec_id: &str,
    pid: u32,
    signal: i32,
) -> Result<()> {
    let info = docker.inspect_exec(exec_id).await.map_err(engine_err)?;
    if info.running != Some(true) {
        return Ok(());
    }
    let script = format!(
        "kill -{signal} \"$(cat {d}/{pid}.pid)\"",
        d = crate::sandbox::STATE_DIR
    );
    let opts = CreateExecOptions {
        cmd: Some(vec!["/bin/sh".to_string(), "-c".to_string(), script]),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        ..Default::default()
    };
    let created = docker
        .create_exec(container_id, opts)
        .await
        .map_err(engine_err)?;
    docker
        .start_exec(
            &created.id,
            Some(StartExecOptions {
                detach: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(engine_err)?;
    Ok(())
}

#[async_trait]
impl Process for DockerProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn output(&mut self) -> OutputStreamBox {
        match self.output.take() {
            Some(raw) => Box::pin(raw.filter_map(|item| async move {
                match item {
                    Ok(LogOutput::StdOut { message }) | Ok(LogOutput::Console { message }) => {
                        Some(Ok(OutputChunk {
                            stream: OutputStream::Stdout,
                            data: message,
                        }))
                    }
                    Ok(LogOutput::StdErr { message }) => Some(Ok(OutputChunk {
                        stream: OutputStream::Stderr,
                        data: message,
                    })),
                    Ok(LogOutput::StdIn { .. }) => None,
                    Err(e) => Some(Err(engine_err(e))),
                }
            })),
            None => Box::pin(futures::stream::empty()),
        }
    }

    fn stdin(&mut self) -> Option<StdinBox> {
        self.input.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        wait_exec(&self.docker, &self.exec_id, self.started).await
    }

    async fn kill(&mut self, signal: i32) -> Result<()> {
        kill_exec(
            &self.docker,
            &self.container_id,
            &self.exec_id,
            self.pid,
            signal,
        )
        .await
    }
}

pub struct DockerPty {
    docker: Arc<Docker>,
    container_id: String,
    exec_id: String,
    pid: u32,
    output: Option<RawOutput>,
    input: Option<StdinBox>,
    started: Instant,
}

impl DockerPty {
    pub(crate) fn new(
        docker: Arc<Docker>,
        container_id: String,
        exec_id: String,
        pid: u32,
        started: StartExecResults,
    ) -> Self {
        let (output, input) = match started {
            StartExecResults::Attached { output, input } => (Some(output), Some(input)),
            StartExecResults::Detached => (None, None),
        };
        Self {
            docker,
            container_id,
            exec_id,
            pid,
            output,
            input,
            started: Instant::now(),
        }
    }
}

#[async_trait]
impl Pty for DockerPty {
    fn output(&mut self) -> ByteStreamBox {
        match self.output.take() {
            Some(raw) => Box::pin(raw.filter_map(|item| async move {
                match item {
                    Ok(LogOutput::StdIn { .. }) => None,
                    Ok(other) => Some(Ok(other.into_bytes())),
                    Err(e) => Some(Err(engine_err(e))),
                }
            })),
            None => Box::pin(futures::stream::empty()),
        }
    }

    async fn write(&mut self, data: Bytes) -> Result<()> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| Error::Guest("pty stdin closed".into()))?;
        input.write_all(&data).await?;
        input.flush().await?;
        Ok(())
    }

    async fn resize(&mut self, size: PtySize) -> Result<()> {
        let opts = bollard::query_parameters::ResizeExecOptionsBuilder::new()
            .h(i32::from(size.rows))
            .w(i32::from(size.cols))
            .build();
        self.docker
            .resize_exec(&self.exec_id, opts)
            .await
            .map_err(engine_err)
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        wait_exec(&self.docker, &self.exec_id, self.started).await
    }

    async fn close(&mut self) -> Result<()> {
        self.input.take();
        kill_exec(&self.docker, &self.container_id, &self.exec_id, self.pid, 9).await
    }
}
