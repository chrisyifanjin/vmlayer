//! Turning table events into `vmlayer.v1.ExecEvent`s and following a process
//! to its exit. Used by both the daemon's and the guest agent's `Processes`
//! service.

use std::sync::Arc;

use tokio::sync::mpsc;
use tonic::Status;
use vmlayer_core::{ExitStatus, OutputChunk, OutputStream};
use vmlayer_proto::v1 as pb;

use crate::table::{Entry, Event};

pub fn exit_status(e: ExitStatus) -> pb::ExitStatus {
    pb::ExitStatus {
        code: e.code,
        signal: e.signal,
        duration: prost_types::Duration::try_from(e.duration).ok(),
        timed_out: e.timed_out,
    }
}

pub fn output_chunk(c: &OutputChunk) -> pb::OutputChunk {
    pb::OutputChunk {
        stream: match c.stream {
            OutputStream::Stdout => pb::OutputStream::Stdout,
            OutputStream::Stderr => pb::OutputStream::Stderr,
        }
        .into(),
        data: c.data.to_vec(),
    }
}

pub fn event(ev: &Event) -> pb::ExecEvent {
    let e = match ev {
        Event::Output(c) => pb::exec_event::Event::Output(output_chunk(c)),
        Event::Dropped(n) => pb::exec_event::Event::DroppedBytes(*n),
        Event::Exited(st) => pb::exec_event::Event::Exited(exit_status(*st)),
    };
    pb::ExecEvent { event: Some(e) }
}

/// Replay history, then follow live output until exit. Ends when the client
/// goes away or the process exits.
pub async fn follow(entry: Arc<Entry>, tx: mpsc::Sender<Result<pb::ExecEvent, Status>>) {
    let (history, mut live) = entry.subscribe().await;
    let mut done = false;
    for ev in &history {
        if matches!(ev, Event::Exited(_)) {
            done = true;
        }
        if tx.send(Ok(event(ev))).await.is_err() {
            return;
        }
    }
    if done {
        return;
    }
    loop {
        match live.recv().await {
            Ok(ev) => {
                let exited = matches!(ev, Event::Exited(_));
                if tx.send(Ok(event(&ev))).await.is_err() || exited {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let ev = pb::ExecEvent {
                    event: Some(pb::exec_event::Event::DroppedBytes(n)),
                };
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                if let Some(st) = entry.exited() {
                    let _ = tx.send(Ok(event(&Event::Exited(st)))).await;
                }
                return;
            }
        }
    }
}
