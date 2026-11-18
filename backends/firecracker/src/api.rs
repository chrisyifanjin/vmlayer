//! Firecracker's REST API over its unix socket. Only the calls this backend
//! makes; shapes follow the 1.16 OpenAPI spec.

use std::path::Path;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use vmlayer_core::{Error, Result};

use crate::BACKEND_NAME;

pub fn err(message: impl Into<String>) -> Error {
    Error::Engine {
        backend: BACKEND_NAME,
        message: message.into(),
        source: None,
    }
}

#[derive(Clone)]
pub struct Client {
    socket: std::path::PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn request(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(u16, String)> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|e| err(format!("connecting to {}: {e}", self.socket.display())))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| err(format!("api handshake: {e}")))?;
        tokio::spawn(conn);
        let mut req = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost")
            .header("Accept", "application/json");
        if body.is_some() {
            req = req.header("Content-Type", "application/json");
        }
        let req = req
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| err(e.to_string()))?;
        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| err(format!("api {path}: {e}")))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| err(e.to_string()))?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    }

    async fn expect_ok(
        &self,
        method: hyper::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<String> {
        let (status, text) = self.request(method.clone(), path, body).await?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            let msg = serde_json::from_str::<FcError>(&text)
                .map(|e| e.fault_message)
                .unwrap_or(text);
            Err(err(format!("{method} {path} -> {status}: {msg}")))
        }
    }

    pub async fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body).map_err(|e| err(e.to_string()))?;
        self.expect_ok(hyper::Method::PUT, path, Some(json))
            .await
            .map(|_| ())
    }

    /// Point a drive at its backing file again after boot or restore;
    /// Firecracker re-reads the size and notifies the guest.
    pub async fn update_drive(&self, drive_id: &str, path_on_host: &str) -> Result<()> {
        #[derive(Serialize)]
        struct DriveUpdate<'a> {
            drive_id: &'a str,
            path_on_host: &'a str,
        }
        self.patch(
            &format!("/drives/{drive_id}"),
            &DriveUpdate {
                drive_id,
                path_on_host,
            },
        )
        .await
    }

    pub async fn patch<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body).map_err(|e| err(e.to_string()))?;
        self.expect_ok(hyper::Method::PATCH, path, Some(json))
            .await
            .map(|_| ())
    }

    pub async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let text = self.expect_ok(hyper::Method::GET, path, None).await?;
        serde_json::from_str(&text).map_err(|e| err(format!("decoding {path}: {e}")))
    }

    /// Poll until the API socket answers.
    pub async fn wait_ready(&self, timeout: std::time::Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.socket.exists() {
                if let Ok((200, _)) = self.request(hyper::Method::GET, "/", None).await {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(err(format!(
                    "firecracker API did not come up within {timeout:?}"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    // -- typed calls --------------------------------------------------------

    pub async fn boot_source(&self, kernel: &str, boot_args: &str) -> Result<()> {
        self.put(
            "/boot-source",
            &BootSource {
                kernel_image_path: kernel,
                boot_args,
            },
        )
        .await
    }

    pub async fn drive(&self, d: &Drive<'_>) -> Result<()> {
        self.put(&format!("/drives/{}", d.drive_id), d).await
    }

    pub async fn network_interface(
        &self,
        iface_id: &str,
        host_dev_name: &str,
        guest_mac: &str,
        limiter: Option<RateLimiter>,
    ) -> Result<()> {
        self.put(
            &format!("/network-interfaces/{iface_id}"),
            &NetworkInterface {
                iface_id,
                host_dev_name,
                guest_mac,
                rx_rate_limiter: limiter,
                tx_rate_limiter: limiter,
            },
        )
        .await
    }

    pub async fn machine_config(&self, vcpu_count: u32, mem_size_mib: u32) -> Result<()> {
        self.put(
            "/machine-config",
            &MachineConfig {
                vcpu_count,
                mem_size_mib,
                smt: false,
                // Full snapshots do not use dirty-page tracking. Keeping it
                // enabled makes every guest write pay KVM logging overhead,
                // including the cold boot that dominates sandbox creation.
                track_dirty_pages: false,
            },
        )
        .await
    }

    pub async fn vsock(&self, guest_cid: u32, uds_path: &str) -> Result<()> {
        self.put(
            "/vsock",
            &Vsock {
                guest_cid,
                uds_path,
            },
        )
        .await
    }

    pub async fn start(&self) -> Result<()> {
        self.put(
            "/actions",
            &Action {
                action_type: "InstanceStart",
            },
        )
        .await
    }

    pub async fn pause(&self) -> Result<()> {
        self.patch("/vm", &VmState { state: "Paused" }).await
    }

    pub async fn resume(&self) -> Result<()> {
        self.patch("/vm", &VmState { state: "Resumed" }).await
    }

    pub async fn snapshot_create(
        &self,
        snapshot_path: &str,
        mem_file_path: &str,
        diff: bool,
    ) -> Result<()> {
        self.put(
            "/snapshot/create",
            &SnapshotCreate {
                snapshot_type: if diff { "Diff" } else { "Full" },
                snapshot_path,
                mem_file_path,
            },
        )
        .await
    }

    pub async fn snapshot_load(&self, load: &SnapshotLoad<'_>) -> Result<()> {
        self.put("/snapshot/load", load).await
    }

    pub async fn info(&self) -> Result<InstanceInfo> {
        self.get("/").await
    }
}

#[derive(Deserialize)]
struct FcError {
    fault_message: String,
}

#[derive(Serialize)]
struct BootSource<'a> {
    kernel_image_path: &'a str,
    boot_args: &'a str,
}

#[derive(Serialize)]
pub struct Drive<'a> {
    pub drive_id: &'a str,
    pub path_on_host: &'a str,
    pub is_root_device: bool,
    pub is_read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RateLimiter>,
}

/// Firecracker's token bucket: `size` tokens, refilled every `refill_time` ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TokenBucket {
    pub size: u64,
    pub refill_time: u64,
}

/// A device rate limiter: bytes per second and/or operations per second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RateLimiter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth: Option<TokenBucket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ops: Option<TokenBucket>,
}

impl RateLimiter {
    fn per_second(n: u64) -> TokenBucket {
        TokenBucket {
            size: n.max(1),
            refill_time: 1000,
        }
    }

    /// For the NIC, applied to each direction.
    pub fn network(limits: &vmlayer_core::IoLimits) -> Option<Self> {
        let mbps = limits.network_mbps?;
        Some(Self {
            bandwidth: Some(Self::per_second(u64::from(mbps) * 1_000_000 / 8)),
            ops: None,
        })
    }

    /// For each disk.
    pub fn disk(limits: &vmlayer_core::IoLimits) -> Option<Self> {
        let limiter = Self {
            bandwidth: limits
                .disk_mbps
                .map(|m| Self::per_second(u64::from(m) * 1_000_000)),
            ops: limits.disk_iops.map(|n| Self::per_second(u64::from(n))),
        };
        (limiter != Self::default()).then_some(limiter)
    }
}

#[derive(Serialize)]
struct NetworkInterface<'a> {
    iface_id: &'a str,
    host_dev_name: &'a str,
    guest_mac: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rx_rate_limiter: Option<RateLimiter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_rate_limiter: Option<RateLimiter>,
}

#[derive(Serialize)]
struct MachineConfig {
    vcpu_count: u32,
    mem_size_mib: u32,
    smt: bool,
    track_dirty_pages: bool,
}

#[derive(Serialize)]
struct Vsock<'a> {
    guest_cid: u32,
    uds_path: &'a str,
}

#[derive(Serialize)]
struct Action<'a> {
    action_type: &'a str,
}

#[derive(Serialize)]
struct VmState<'a> {
    state: &'a str,
}

#[derive(Serialize)]
struct SnapshotCreate<'a> {
    snapshot_type: &'a str,
    snapshot_path: &'a str,
    mem_file_path: &'a str,
}

#[derive(Serialize)]
pub struct MemBackend<'a> {
    pub backend_type: &'a str,
    pub backend_path: &'a str,
}

#[derive(Serialize)]
pub struct NetworkOverride<'a> {
    pub iface_id: &'a str,
    pub host_dev_name: &'a str,
}

#[derive(Serialize)]
pub struct SnapshotLoad<'a> {
    pub snapshot_path: &'a str,
    pub mem_backend: MemBackend<'a>,
    pub enable_diff_snapshots: bool,
    pub resume_vm: bool,
    /// Only sent when non-empty: every VM's tap has the same name inside its
    /// own namespace, so the snapshot's interface config is already right.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub network_overrides: Vec<NetworkOverride<'a>>,
}

#[derive(Deserialize, Debug)]
pub struct InstanceInfo {
    pub state: String,
    pub vmm_version: String,
}

/// `firecracker --version`, for `SnapshotRef::engine_version`.
pub async fn version(bin: &Path) -> Result<String> {
    let out = tokio::process::Command::new(bin)
        .arg("--version")
        .output()
        .await
        .map_err(|e| err(format!("running {}: {e}", bin.display())))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().last())
        .unwrap_or("unknown")
        .trim_start_matches('v')
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-request fake Firecracker API on a unix socket.
    async fn fake(
        status: &'static str,
        body: &'static str,
    ) -> (std::path::PathBuf, tokio::task::JoinHandle<String>) {
        let dir = tempfile::tempdir().unwrap().keep();
        let sock = dir.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        (sock, task)
    }

    #[tokio::test]
    async fn put_sends_json_and_accepts_204() {
        let (sock, task) = fake("204 No Content", "").await;
        let c = Client::new(&sock);
        c.machine_config(2, 1024).await.unwrap();
        let req = task.await.unwrap();
        assert!(req.starts_with("PUT /machine-config HTTP/1.1"));
        assert!(req.contains(r#""vcpu_count":2"#) && req.contains(r#""mem_size_mib":1024"#));
    }

    #[tokio::test]
    async fn errors_surface_fault_message() {
        let (sock, _task) = fake("400 Bad Request", r#"{"fault_message":"bad drive"}"#).await;
        let c = Client::new(&sock);
        let e = c.start().await.unwrap_err();
        assert!(e.to_string().contains("bad drive"), "{e}");
    }

    #[tokio::test]
    async fn get_decodes() {
        let (sock, _task) = fake(
            "200 OK",
            r#"{"id":"x","state":"Running","vmm_version":"1.16.1"}"#,
        )
        .await;
        let c = Client::new(&sock);
        let info = c.info().await.unwrap();
        assert_eq!(info.vmm_version, "1.16.1");
    }

    #[test]
    fn snapshot_load_serializes_overrides() {
        let l = SnapshotLoad {
            snapshot_path: "/snap/vmstate",
            mem_backend: MemBackend {
                backend_type: "File",
                backend_path: "/snap/mem",
            },
            enable_diff_snapshots: false,
            resume_vm: true,
            network_overrides: vec![NetworkOverride {
                iface_id: "eth0",
                host_dev_name: "vmltap3",
            }],
        };
        let j = serde_json::to_value(&l).unwrap();
        assert_eq!(j["mem_backend"]["backend_type"], "File");
        assert_eq!(j["network_overrides"][0]["host_dev_name"], "vmltap3");
    }
}
