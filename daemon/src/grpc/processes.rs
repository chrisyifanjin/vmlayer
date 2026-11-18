use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use vmlayer_core::{ExecRequest, PtySize};
use vmlayer_proto::v1 as pb;
use vmlayer_proto::v1::processes_server::Processes;

use super::convert::{self, status};
use super::Services;
use crate::auth::Principal;
use vmlayer_procs::grpc::follow;

type EventStream = Pin<Box<dyn Stream<Item = Result<pb::ExecEvent, Status>> + Send>>;
type PtyStream = Pin<Box<dyn Stream<Item = Result<pb::PtyOutput, Status>> + Send>>;

fn exec_request(
    argv: Vec<String>,
    cwd: String,
    env: std::collections::HashMap<String, String>,
    timeout: Option<prost_types::Duration>,
    pty: Option<PtySize>,
) -> Result<ExecRequest, Status> {
    if argv.is_empty() {
        return Err(Status::invalid_argument("argv is required"));
    }
    Ok(ExecRequest {
        argv,
        cwd: (!cwd.is_empty()).then_some(cwd),
        env: env.into_iter().collect::<BTreeMap<_, _>>(),
        timeout: convert::dur_from(timeout),
        pty,
    })
}

#[tonic::async_trait]
impl Processes for Services {
    type ExecStream = EventStream;
    type OutputStream = EventStream;
    type PtyStream = PtyStream;

    async fn exec(
        &self,
        req: Request<pb::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        convert::require_id(&r.sandbox_id, "sandbox_id")?;
        let er = exec_request(r.argv, r.cwd, r.env, r.timeout, None)?;
        let (_, entry) = self
            .daemon
            .exec(&self.sandbox(&p, &r.sandbox_id)?, er)
            .await
            .map_err(status)?;
        if !r.stdin.is_empty() {
            entry
                .write_stdin(Bytes::from(r.stdin))
                .await
                .map_err(status)?;
        }
        // Exec's own stdin is closed after the initial bytes; interactive
        // use goes through the Stdin RPC on a detached process.
        if !r.detach {
            entry.close_stdin().await.map_err(status)?;
        }
        let (tx, rx) = mpsc::channel(256);
        let started = pb::ExecEvent {
            event: Some(pb::exec_event::Event::Started(pb::Started {
                pid: entry.pid,
            })),
        };
        let _ = tx.send(Ok(started)).await;
        if !r.detach {
            tokio::spawn(follow(entry, tx));
        }
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn output(
        &self,
        req: Request<pb::OutputRequest>,
    ) -> Result<Response<Self::OutputStream>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let (_, entry) = self
            .daemon
            .process(&self.sandbox(&p, &r.sandbox_id)?, r.pid)
            .await
            .map_err(status)?;
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(follow(entry, tx));
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn stdin(
        &self,
        req: Request<Streaming<pb::StdinChunk>>,
    ) -> Result<Response<pb::Empty>, Status> {
        let p = Principal::of(&req);
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("empty stdin stream"))??;
        let (_, entry) = self
            .daemon
            .process(&self.sandbox(&p, &first.sandbox_id)?, first.pid)
            .await
            .map_err(status)?;
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
        let p = Principal::of(&req);
        let r = req.into_inner();
        let (_, entry) = self
            .daemon
            .process(&self.sandbox(&p, &r.sandbox_id)?, r.pid)
            .await
            .map_err(status)?;
        let st = entry
            .wait(convert::dur_from(r.timeout))
            .await
            .map_err(status)?;
        Ok(Response::new(convert::exit_status(st)))
    }

    async fn kill(
        &self,
        req: Request<pb::KillProcessRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let p = Principal::of(&req);
        let r = req.into_inner();
        let (_, entry) = self
            .daemon
            .process(&self.sandbox(&p, &r.sandbox_id)?, r.pid)
            .await
            .map_err(status)?;
        let signal = if r.signal == 0 { 9 } else { r.signal };
        entry.kill(signal).await.map_err(status)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn list(
        &self,
        req: Request<pb::ListProcessesRequest>,
    ) -> Result<Response<pb::ListProcessesResponse>, Status> {
        let p = Principal::of(&req);
        let id = self.sandbox(&p, &req.into_inner().sandbox_id)?;
        let live = self.daemon.live(&id).await.map_err(status)?;
        // Backend-tracked (entrypoint) plus daemon-tracked, deduplicated by pid.
        let mut by_pid: BTreeMap<u32, pb::ProcessInfo> = BTreeMap::new();
        for p in live.handle.processes().await.map_err(status)? {
            by_pid.insert(p.pid, convert::process_info(&p));
        }
        for e in live.procs.running().await {
            by_pid.entry(e.pid).or_insert_with(|| pb::ProcessInfo {
                pid: e.pid,
                argv: e.argv.clone(),
                started_at: convert::ts(e.started_at),
                is_entrypoint: e.is_entrypoint,
            });
        }
        Ok(Response::new(pb::ListProcessesResponse {
            processes: by_pid.into_values().collect(),
        }))
    }

    async fn pty(
        &self,
        req: Request<Streaming<pb::PtyInput>>,
    ) -> Result<Response<Self::PtyStream>, Status> {
        let p = Principal::of(&req);
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
        let er = exec_request(argv, open.cwd, open.env, None, Some(size))?;
        let mut pty = self
            .daemon
            .pty(&self.sandbox(&p, &open.sandbox_id)?, er)
            .await
            .map_err(status)?;
        let mut output = pty.output();
        let pty = Arc::new(Mutex::new(pty));
        let (tx, rx) = mpsc::channel::<Result<pb::PtyOutput, Status>>(256);

        // Output pump.
        let out_tx = tx.clone();
        let out_pty = pty.clone();
        tokio::spawn(async move {
            let _ = out_tx
                .send(Ok(pb::PtyOutput {
                    output: Some(pb::pty_output::Output::Started(pb::Started { pid: 0 })),
                }))
                .await;
            while let Some(item) = output.next().await {
                let msg = match item {
                    Ok(b) => pb::PtyOutput {
                        output: Some(pb::pty_output::Output::Data(b.to_vec())),
                    },
                    Err(e) => {
                        let _ = out_tx.send(Err(status(e))).await;
                        break;
                    }
                };
                if out_tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
            let st = out_pty
                .lock()
                .await
                .wait()
                .await
                .unwrap_or(vmlayer_core::ExitStatus {
                    code: None,
                    signal: Some(9),
                    duration: std::time::Duration::ZERO,
                    timed_out: false,
                });
            let _ = out_tx
                .send(Ok(pb::PtyOutput {
                    output: Some(pb::pty_output::Output::Exited(convert::exit_status(st))),
                }))
                .await;
        });

        // Input pump.
        let in_pty = pty.clone();
        tokio::spawn(async move {
            while let Some(msg) = inbound.next().await {
                let Ok(msg) = msg else { break };
                let res = match msg.input {
                    Some(pb::pty_input::Input::Data(d)) => {
                        in_pty.lock().await.write(Bytes::from(d)).await
                    }
                    Some(pb::pty_input::Input::Resize(s)) => {
                        in_pty
                            .lock()
                            .await
                            .resize(PtySize {
                                cols: s.cols as u16,
                                rows: s.rows as u16,
                            })
                            .await
                    }
                    _ => Ok(()),
                };
                if res.is_err() {
                    break;
                }
            }
            let _ = in_pty.lock().await.close().await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
