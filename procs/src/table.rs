//! Process table: output buffering and fan-out so a client can reconnect to a
//! running process, plus timeouts. Backends hand us a `Process`; we own it
//! from then on.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::{broadcast, watch, Mutex};
use vmlayer_core::backend::StdinBox;
use vmlayer_core::{Error, ExitStatus, OutputChunk, OutputStream, Process, Result};

/// One item of a process's history.
#[derive(Clone, Debug)]
pub enum Event {
    Output(OutputChunk),
    Dropped(u64),
    Exited(ExitStatus),
}

struct Buffer {
    events: VecDeque<Event>,
    bytes: usize,
    cap: usize,
    dropped: u64,
    exited: Option<ExitStatus>,
}

impl Buffer {
    fn push(&mut self, chunk: OutputChunk) {
        self.bytes += chunk.data.len();
        self.events.push_back(Event::Output(chunk));
        while self.bytes > self.cap {
            match self.events.pop_front() {
                Some(Event::Output(c)) => {
                    self.bytes -= c.data.len();
                    self.dropped += c.data.len() as u64;
                }
                Some(other) => self.events.push_front(other),
                None => break,
            }
            if matches!(self.events.front(), Some(Event::Dropped(_)) | None) {
                break;
            }
        }
    }

    fn snapshot(&self) -> Vec<Event> {
        let mut v = Vec::with_capacity(self.events.len() + 2);
        if self.dropped > 0 {
            v.push(Event::Dropped(self.dropped));
        }
        v.extend(self.events.iter().cloned());
        if let Some(st) = self.exited {
            v.push(Event::Exited(st));
        }
        v
    }
}

pub struct Entry {
    pub pid: u32,
    pub argv: Vec<String>,
    pub started_at: SystemTime,
    pub is_entrypoint: bool,
    buffer: Arc<Mutex<Buffer>>,
    live: broadcast::Sender<Event>,
    exit_tx: watch::Sender<Option<ExitStatus>>,
    process: Arc<Mutex<Box<dyn Process>>>,
    stdin: Arc<Mutex<Option<StdinBox>>>,
    timed_out: std::sync::atomic::AtomicBool,
}

impl Entry {
    /// Replay history, then follow live output until exit.
    pub async fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let buf = self.buffer.lock().await;
        let rx = self.live.subscribe();
        (buf.snapshot(), rx)
    }

    pub async fn wait(&self, timeout: Option<Duration>) -> Result<ExitStatus> {
        let mut rx = self.exit_tx.subscribe();
        let fut = async {
            loop {
                if let Some(st) = *rx.borrow_and_update() {
                    return st;
                }
                if rx.changed().await.is_err() {
                    // Sender dropped without exit: treat as killed.
                    return ExitStatus {
                        code: None,
                        signal: Some(9),
                        duration: Duration::ZERO,
                        timed_out: false,
                    };
                }
            }
        };
        match timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| Error::Timeout(t)),
            None => Ok(fut.await),
        }
    }

    pub async fn kill(&self, signal: i32) -> Result<()> {
        self.process.lock().await.kill(signal).await
    }

    pub async fn write_stdin(&self, data: Bytes) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut guard = self.stdin.lock().await;
        let w = guard
            .as_mut()
            .ok_or_else(|| Error::Guest("stdin is closed".into()))?;
        w.write_all(&data).await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn close_stdin(&self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        if let Some(mut w) = self.stdin.lock().await.take() {
            let _ = w.shutdown().await;
        }
        Ok(())
    }

    pub fn exited(&self) -> Option<ExitStatus> {
        *self.exit_tx.borrow()
    }
}

/// Per-sandbox table.
pub struct ProcessTable {
    entries: Mutex<BTreeMap<u32, Arc<Entry>>>,
    cap: usize,
}

impl ProcessTable {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            cap,
        }
    }

    /// Adopt a process the backend just started. Spawns the pump task.
    pub async fn adopt(
        &self,
        mut process: Box<dyn Process>,
        argv: Vec<String>,
        is_entrypoint: bool,
        timeout: Option<Duration>,
    ) -> Arc<Entry> {
        let pid = process.pid();
        let output = process.output();
        let stdin = process.stdin();
        let (live, _) = broadcast::channel(1024);
        let (exit_tx, _) = watch::channel(None);
        let entry = Arc::new(Entry {
            pid,
            argv,
            started_at: SystemTime::now(),
            is_entrypoint,
            buffer: Arc::new(Mutex::new(Buffer {
                events: VecDeque::new(),
                bytes: 0,
                cap: self.cap,
                dropped: 0,
                exited: None,
            })),
            live,
            exit_tx,
            process: Arc::new(Mutex::new(process)),
            stdin: Arc::new(Mutex::new(stdin)),
            timed_out: std::sync::atomic::AtomicBool::new(false),
        });
        self.entries.lock().await.insert(pid, entry.clone());

        // Pump: output -> buffer + live; then wait -> exit.
        let pump = entry.clone();
        tokio::spawn(async move {
            let mut output = output;
            while let Some(item) = output.next().await {
                match item {
                    Ok(chunk) => {
                        pump.buffer.lock().await.push(chunk.clone());
                        let _ = pump.live.send(Event::Output(chunk));
                    }
                    Err(e) => {
                        let msg = Bytes::from(format!("\n[vmlayer] output error: {e}\n"));
                        let chunk = OutputChunk {
                            stream: OutputStream::Stderr,
                            data: msg,
                        };
                        pump.buffer.lock().await.push(chunk.clone());
                        let _ = pump.live.send(Event::Output(chunk));
                        break;
                    }
                }
            }
            let mut status = match pump.process.lock().await.wait().await {
                Ok(st) => st,
                Err(e) => {
                    tracing::warn!(pid, error = %e, "wait failed");
                    ExitStatus {
                        code: None,
                        signal: Some(9),
                        duration: Duration::ZERO,
                        timed_out: false,
                    }
                }
            };
            status.timed_out = pump.timed_out.load(std::sync::atomic::Ordering::SeqCst);
            pump.buffer.lock().await.exited = Some(status);
            let _ = pump.live.send(Event::Exited(status));
            let _ = pump.exit_tx.send(Some(status));
        });

        if let Some(t) = timeout {
            let victim = entry.clone();
            tokio::spawn(async move {
                if victim.wait(Some(t)).await.is_err() {
                    tracing::info!(pid, timeout = ?t, "process timed out; killing");
                    victim
                        .timed_out
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = victim.kill(9).await;
                }
            });
        }
        entry
    }

    pub async fn get(&self, pid: u32) -> Option<Arc<Entry>> {
        self.entries.lock().await.get(&pid).cloned()
    }

    pub async fn running(&self) -> Vec<Arc<Entry>> {
        self.entries
            .lock()
            .await
            .values()
            .filter(|e| e.exited().is_none())
            .cloned()
            .collect()
    }

    /// Drop finished entries older than `keep` to bound memory.
    pub async fn gc(&self, keep: usize) {
        let mut entries = self.entries.lock().await;
        let finished: Vec<u32> = entries
            .iter()
            .filter(|(_, e)| e.exited().is_some())
            .map(|(pid, _)| *pid)
            .collect();
        if finished.len() > keep {
            for pid in &finished[..finished.len() - keep] {
                entries.remove(pid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use vmlayer_core::backend::OutputStreamBox;

    struct Fake {
        chunks: Vec<&'static str>,
        status: ExitStatus,
    }

    #[async_trait]
    impl Process for Fake {
        fn pid(&self) -> u32 {
            7
        }
        fn output(&mut self) -> OutputStreamBox {
            let items: Vec<Result<OutputChunk>> = self
                .chunks
                .iter()
                .map(|c| {
                    Ok(OutputChunk {
                        stream: OutputStream::Stdout,
                        data: Bytes::from_static(c.as_bytes()),
                    })
                })
                .collect();
            Box::pin(futures::stream::iter(items))
        }
        fn stdin(&mut self) -> Option<StdinBox> {
            None
        }
        async fn wait(&mut self) -> Result<ExitStatus> {
            Ok(self.status)
        }
        async fn kill(&mut self, _signal: i32) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn buffers_and_replays() {
        let table = ProcessTable::new(1024);
        let st = ExitStatus {
            code: Some(0),
            signal: None,
            duration: Duration::from_millis(1),
            timed_out: false,
        };
        let e = table
            .adopt(
                Box::new(Fake {
                    chunks: vec!["a", "b"],
                    status: st,
                }),
                vec!["x".into()],
                false,
                None,
            )
            .await;
        assert_eq!(
            e.wait(Some(Duration::from_secs(1))).await.unwrap().code,
            Some(0)
        );
        let (history, _) = e.subscribe().await;
        let text: Vec<String> = history
            .iter()
            .filter_map(|ev| match ev {
                Event::Output(c) => Some(String::from_utf8_lossy(&c.data).into_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(text, vec!["a", "b"]);
        assert!(matches!(history.last(), Some(Event::Exited(_))));
        assert!(table.running().await.is_empty());
    }

    #[tokio::test]
    async fn drops_oldest_when_over_cap() {
        let table = ProcessTable::new(3);
        let st = ExitStatus {
            code: Some(0),
            signal: None,
            duration: Duration::ZERO,
            timed_out: false,
        };
        let e = table
            .adopt(
                Box::new(Fake {
                    chunks: vec!["aa", "bb", "cc"],
                    status: st,
                }),
                vec![],
                false,
                None,
            )
            .await;
        e.wait(Some(Duration::from_secs(1))).await.unwrap();
        let (history, _) = e.subscribe().await;
        assert!(matches!(history.first(), Some(Event::Dropped(n)) if *n >= 2));
    }
}
