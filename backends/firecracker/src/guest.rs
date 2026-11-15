//! Talking to the guest agent over Firecracker's vsock bridge.
//!
//! Firecracker exposes the guest's vsock as a unix socket on the host. To
//! reach guest port P, connect to it, send `CONNECT P\n`, and read
//! `OK <host_port>\n`; the stream is then end to end. tonic's channel takes
//! that stream through a custom connector.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tonic::transport::{Channel, Endpoint, Uri};
use vmlayer_core::Result;
use vmlayer_proto::guest::v1::guest_client::GuestClient;
use vmlayer_proto::v1::files_client::FilesClient;
use vmlayer_proto::v1::processes_client::ProcessesClient;

use crate::api::err;

/// Perform the CONNECT handshake and hand back the raw stream.
pub async fn connect_vsock(uds: &Path, port: u32) -> std::io::Result<tokio::net::UnixStream> {
    let stream = tokio::net::UnixStream::connect(uds).await?;
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if !parse_ok(&line) {
        return Err(std::io::Error::other(format!(
            "vsock CONNECT {port}: {}",
            line.trim()
        )));
    }
    Ok(reader.into_inner())
}

pub fn parse_ok(line: &str) -> bool {
    let mut it = line.split_whitespace();
    it.next() == Some("OK") && it.next().map(|p| p.parse::<u32>().is_ok()).unwrap_or(false)
}

#[derive(Clone)]
pub struct Agent {
    pub processes: ProcessesClient<Channel>,
    pub files: FilesClient<Channel>,
    pub guest: GuestClient<Channel>,
}

impl Agent {
    pub async fn connect(uds: PathBuf, port: u32) -> Result<Self> {
        let channel = Endpoint::try_from("http://[::]:1")
            .map_err(|e| err(e.to_string()))?
            .connect_timeout(Duration::from_secs(5))
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let uds = uds.clone();
                async move { connect_vsock(&uds, port).await.map(TokioIo::new) }
            }))
            .await
            .map_err(|e| err(format!("guest agent channel: {e}")))?;
        Ok(Self {
            processes: ProcessesClient::new(channel.clone()),
            files: FilesClient::new(channel.clone()),
            guest: GuestClient::new(channel),
        })
    }

    /// Connect and wait for `Ready`, retrying while the guest boots.
    pub async fn wait_ready(uds: PathBuf, port: u32, timeout: Duration) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last;
        loop {
            match Self::connect(uds.clone(), port).await {
                Ok(mut a) => match a.guest.ready(vmlayer_proto::v1::Empty {}).await {
                    Ok(r) => {
                        tracing::debug!(agent = %r.get_ref().agent_version, kernel = %r.get_ref().kernel, "guest agent ready");
                        return Ok(a);
                    }
                    Err(e) => last = e.to_string(),
                },
                Err(e) => last = e.to_string(),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(err(format!(
                    "guest agent not ready after {timeout:?}: {last}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_handshake() {
        assert!(parse_ok("OK 1024\n"));
        assert!(!parse_ok("OK\n"));
        assert!(!parse_ok("ERR\n"));
    }

    #[tokio::test]
    async fn handshake_against_fake_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("v.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let mut r = BufReader::new(s);
            let mut line = String::new();
            r.read_line(&mut line).await.unwrap();
            assert_eq!(line, "CONNECT 5200\n");
            r.get_mut().write_all(b"OK 4096\n").await.unwrap();
        });
        connect_vsock(&sock, 5200).await.unwrap();
    }
}
