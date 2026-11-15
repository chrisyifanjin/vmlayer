//! `Process` and `Pty` as remote handles on the guest agent's gRPC services.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use vmlayer_core::backend::{ByteStreamBox, OutputStreamBox, StdinBox};
use vmlayer_core::{Error, ExitStatus, OutputChunk, OutputStream, Process, Pty, PtySize, Result};
use vmlayer_proto::v1 as pb;

use crate::api::err;
use crate::guest::{is_transport_error, Agent, Link};

pub fn exit_status(st: &pb::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: st.code,
        signal: st.signal,
        duration: st
            .duration
            .and_then(|d| std::time::Duration::try_from(d).ok())
            .unwrap_or_default(),
        timed_out: st.timed_out,
    }
}

fn chunk(c: &pb::OutputChunk) -> OutputChunk {
    OutputChunk {
        stream: if c.stream == pb::OutputStream::Stderr as i32 {
            OutputStream::Stderr
        } else {
            OutputStream::Stdout
        },
        data: Bytes::from(c.data.clone()),
    }
}

/// Writer that feeds the client-streaming Stdin RPC.
struct StdinWriter {
    tx: Option<mpsc::Sender<pb::StdinChunk>>,
}

impl tokio::io::AsyncWrite for StdinWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &self.tx {
            Some(tx) => match tx.try_send(pb::StdinChunk {
                data: buf.to_vec(),
                ..Default::default()
            }) {
                Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Back off; the task will be re-polled.
                    let waker = _cx.waker().clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        waker.wake();
                    });
                    std::task::Poll::Pending
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    std::task::Poll::Ready(Err(std::io::Error::other("stdin closed")))
                }
            },
            None => std::task::Poll::Ready(Err(std::io::Error::other("stdin closed"))),
        }
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.tx.take();
        std::task::Poll::Ready(Ok(()))
    }
}

pub struct RemoteProcess {
    link: Arc<Link>,
    pid: u32,
    output: Option<OutputStreamBox>,
    stdin: Option<StdinBox>,
}

type EventStream = tonic::Streaming<pb::ExecEvent>;

async fn open_output(link: &Link, pid: u32) -> std::result::Result<EventStream, tonic::Status> {
    let mut agent = link.agent();
    Ok(agent
        .processes
        .output(pb::OutputRequest {
            pid,
            ..Default::default()
        })
        .await?
        .into_inner())
}

/// Forward a process's output to `tx` until it exits, surviving connection
/// resets: on a reset, reopen the stream on the link's current connection
/// and skip the history the agent replays up to what was already delivered.
async fn pump_output(
    link: Arc<Link>,
    pid: u32,
    mut stream: EventStream,
    tx: mpsc::Sender<Result<OutputChunk>>,
) {
    let mut delivered: u64 = 0;
    let mut skip: u64 = 0;
    loop {
        while let Some(item) = stream.next().await {
            match item {
                Ok(pb::ExecEvent {
                    event: Some(pb::exec_event::Event::Output(c)),
                }) => {
                    let len = c.data.len() as u64;
                    if skip >= len {
                        skip -= len;
                        continue;
                    }
                    let mut out = chunk(&c);
                    if skip > 0 {
                        out.data = out.data.slice(skip as usize..);
                        skip = 0;
                    }
                    delivered += out.data.len() as u64;
                    if tx.send(Ok(out)).await.is_err() {
                        return;
                    }
                }
                Ok(pb::ExecEvent {
                    event: Some(pb::exec_event::Event::DroppedBytes(n)),
                }) => {
                    // The agent no longer has the oldest bytes; they were ours already.
                    skip = skip.saturating_sub(n);
                }
                Ok(pb::ExecEvent {
                    event: Some(pb::exec_event::Event::Exited(_)),
                }) => return,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // The stream ended without an exit: the connection was reset.
        let mut delay = std::time::Duration::from_millis(50);
        stream = loop {
            if link.is_dead() {
                return;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_secs(1));
            match open_output(&link, pid).await {
                Ok(s) => break s,
                Err(e) if is_transport_error(&e) => continue,
                Err(_) => return, // the agent no longer knows the process
            }
        };
        skip = delivered;
    }
}

impl RemoteProcess {
    /// Start a detached process in the guest and attach to its output.
    pub async fn spawn(link: Arc<Link>, req: pb::ExecRequest) -> Result<Self> {
        let req = pb::ExecRequest {
            detach: true,
            ..req
        };
        let mut agent = link.agent();
        let mut started = agent
            .processes
            .exec(req)
            .await
            .map_err(|e| err(format!("exec: {}", e.message())))?
            .into_inner();
        let pid = match started.next().await {
            Some(Ok(pb::ExecEvent {
                event: Some(pb::exec_event::Event::Started(s)),
            })) => s.pid,
            other => return Err(err(format!("exec did not report a pid: {other:?}"))),
        };
        Self::attach(link, pid).await
    }

    /// Attach to an existing guest process by pid.
    pub async fn attach(link: Arc<Link>, pid: u32) -> Result<Self> {
        let first = open_output(&link, pid)
            .await
            .map_err(|e| Error::Guest(e.message().to_string()))?;
        let (out_tx, out_rx) = mpsc::channel::<Result<OutputChunk>>(256);
        tokio::spawn(pump_output(link.clone(), pid, first, out_tx));
        // Stdin: a channel pumped into the Stdin RPC. It does not survive a
        // snapshot (the connection is reset); output and wait do.
        let (tx, rx) = mpsc::channel::<pb::StdinChunk>(64);
        let mut stdin_agent = link.agent();
        tokio::spawn(async move {
            let first = pb::StdinChunk {
                pid,
                ..Default::default()
            };
            let stream = tokio_stream::iter(std::iter::once(first)).chain(ReceiverStream::new(rx));
            let _ = stdin_agent.processes.stdin(stream).await;
        });
        Ok(Self {
            link,
            pid,
            output: Some(Box::pin(ReceiverStream::new(out_rx))),
            stdin: Some(Box::pin(StdinWriter { tx: Some(tx) })),
        })
    }
}

#[async_trait]
impl Process for RemoteProcess {
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
        let mut delay = std::time::Duration::from_millis(50);
        loop {
            let mut agent = self.link.agent();
            match agent
                .processes
                .wait(pb::WaitRequest {
                    pid: self.pid,
                    ..Default::default()
                })
                .await
            {
                Ok(st) => return Ok(exit_status(st.get_ref())),
                // A snapshot reset the connection (or the VM is paused):
                // the process is still there, ask again on the new one.
                Err(e) if is_transport_error(&e) && !self.link.is_dead() => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(1));
                }
                Err(e) => return Err(err(format!("wait: {}", e.message()))),
            }
        }
    }

    async fn kill(&mut self, signal: i32) -> Result<()> {
        let mut agent = self.link.agent();
        agent
            .processes
            .kill(pb::KillProcessRequest {
                pid: self.pid,
                signal,
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(|e| err(format!("kill: {}", e.message())))
    }
}

pub struct RemotePty {
    input: mpsc::Sender<pb::PtyInput>,
    output: Option<ByteStreamBox>,
    exit: Arc<Mutex<Option<ExitStatus>>>,
    exit_rx: Option<tokio::sync::oneshot::Receiver<ExitStatus>>,
}

impl RemotePty {
    pub async fn open(mut agent: Agent, open: pb::PtyOpen) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<pb::PtyInput>(64);
        tx.send(pb::PtyInput {
            input: Some(pb::pty_input::Input::Open(open)),
        })
        .await
        .map_err(|_| err("pty channel closed"))?;
        let resp = agent
            .processes
            .pty(ReceiverStream::new(rx))
            .await
            .map_err(|e| err(format!("pty: {e}")))?
            .into_inner();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let exit = Arc::new(Mutex::new(None));
        let exit_slot = exit.clone();
        let (out_tx, out_rx) = mpsc::channel::<Result<Bytes>>(256);
        tokio::spawn(async move {
            let mut resp = resp;
            let mut exit_tx = Some(exit_tx);
            while let Some(item) = resp.next().await {
                match item {
                    Ok(pb::PtyOutput {
                        output: Some(pb::pty_output::Output::Data(d)),
                    }) => {
                        if out_tx.send(Ok(Bytes::from(d))).await.is_err() {
                            break;
                        }
                    }
                    Ok(pb::PtyOutput {
                        output: Some(pb::pty_output::Output::Exited(st)),
                    }) => {
                        let st = exit_status(&st);
                        *exit_slot.lock().await = Some(st);
                        if let Some(tx) = exit_tx.take() {
                            let _ = tx.send(st);
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = out_tx.send(Err(Error::Guest(e.to_string()))).await;
                        break;
                    }
                }
            }
        });
        Ok(Self {
            input: tx,
            output: Some(Box::pin(ReceiverStream::new(out_rx))),
            exit,
            exit_rx: Some(exit_rx),
        })
    }
}

#[async_trait]
impl Pty for RemotePty {
    fn output(&mut self) -> ByteStreamBox {
        self.output
            .take()
            .unwrap_or_else(|| Box::pin(futures::stream::empty()))
    }

    async fn write(&mut self, data: Bytes) -> Result<()> {
        self.input
            .send(pb::PtyInput {
                input: Some(pb::pty_input::Input::Data(data.to_vec())),
            })
            .await
            .map_err(|_| err("pty closed"))
    }

    async fn resize(&mut self, size: PtySize) -> Result<()> {
        self.input
            .send(pb::PtyInput {
                input: Some(pb::pty_input::Input::Resize(pb::PtySize {
                    cols: size.cols.into(),
                    rows: size.rows.into(),
                })),
            })
            .await
            .map_err(|_| err("pty closed"))
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(st) = *self.exit.lock().await {
            return Ok(st);
        }
        match self.exit_rx.take() {
            Some(rx) => rx
                .await
                .map_err(|_| err("pty ended without an exit status")),
            None => Err(err("pty exit already consumed")),
        }
    }

    async fn close(&mut self) -> Result<()> {
        // Dropping the sender ends the input stream; the agent kills the child.
        let (tx, _) = mpsc::channel(1);
        self.input = tx;
        Ok(())
    }
}
