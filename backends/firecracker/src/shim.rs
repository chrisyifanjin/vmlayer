//! `vmlayer-shim`: one helper process per microVM (decision 0019).
//!
//! The daemon used to sit in the data path of running VMs: it served lazily
//! restored volumes over NBD, filled restored memory, ran each VM's DNS
//! proxy and expose proxies, and enforced lease deadlines. A VM that kept
//! running while the daemon was down lost its disk reads, its DNS and its
//! fencing. Following containerd's shim v2, each VM now gets a small process
//! that owns those services and lives exactly as long as the VM:
//!
//! - it starts the jailer (and so Firecracker) as its own child, in its own
//!   session, so a daemon stop or crash does not touch it;
//! - it serves lazy volumes (block device + background download), restore
//!   memory population, the egress DNS proxy and the expose proxies;
//! - it holds the VM's fencing deadline and kills the VM when the deadline
//!   passes without being extended, whether or not the daemon is alive;
//! - when Firecracker exits it records how, releases everything and exits.
//!
//! The daemon talks to it over a Unix socket next to the jail (one JSON
//! request and one JSON response per connection) and re-attaches to it after
//! a restart. The spec it starts from is a JSON file written by the backend.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use vmlayer_core::{EgressPolicy, Error, Result};

use crate::api::err;
use crate::net;

/// Restore memory over userfaultfd (decision 0013).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UffdSpec {
    pub socket: PathBuf,
    pub mem: PathBuf,
}

/// The egress DNS proxy (decision 0012).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DnsSpec {
    pub listen: SocketAddr,
    pub netns: String,
    pub upstreams: Vec<SocketAddr>,
    pub egress: EgressPolicy,
}

/// A volume restored lazily from the blob store (decision 0016).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LazySpec {
    /// Device node to create for the VM (its drive path in the jail).
    pub node: PathBuf,
    /// The volume's sparse local image.
    pub image: PathBuf,
    pub snapshot: String,
    pub store_url: String,
    pub store_options: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShimSpec {
    pub id: String,
    /// Full jailer command line.
    pub jailer: Vec<String>,
    pub console: PathBuf,
    /// Firecracker's pid is written here.
    pub pid_file: PathBuf,
    /// Control socket.
    pub control: PathBuf,
    /// How the VM ended, written when it does.
    pub exit_file: PathBuf,
    pub jail_uid: u32,
    pub jail_gid: u32,
    /// Address `expose` proxies connect to (the VM's namespace end).
    pub expose_target: Ipv4Addr,
    #[serde(default)]
    pub uffd: Option<UffdSpec>,
    #[serde(default)]
    pub dns: Option<DnsSpec>,
    #[serde(default)]
    pub lazy: Vec<LazySpec>,
    /// Kill the VM at this time (ms since the epoch) unless extended.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Move (or clear) the fencing deadline.
    Extend {
        deadline_ms: Option<u64>,
    },
    Expose {
        port: u16,
    },
    Unexpose {
        port: u16,
    },
    /// Kill the VM; the shim exits once it is gone.
    Stop,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub host_port: Option<u16>,
    #[serde(default)]
    pub exposed: BTreeMap<u16, u16>,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// Chunks each lazy volume still has to download.
    #[serde(default)]
    pub lazy_missing: Vec<usize>,
}

/// How the VM ended.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Killed by the shim because the fencing deadline passed.
    pub fenced: bool,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Talks to a running shim.
#[derive(Clone, Debug)]
pub struct Client {
    pub socket: PathBuf,
}

impl Client {
    pub async fn call(&self, req: &Request) -> Result<Response> {
        let mut stream =
            tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&self.socket))
                .await
                .map_err(|_| err("shim did not answer"))?
                .map_err(|e| err(format!("shim at {}: {e}", self.socket.display())))?;
        let mut line = serde_json::to_vec(req).map_err(|e| err(e.to_string()))?;
        line.push(b'\n');
        stream.write_all(&line).await?;
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut reply))
            .await
            .map_err(|_| err("shim did not answer in time"))??;
        let resp: Response =
            serde_json::from_str(&reply).map_err(|e| err(format!("shim reply: {e}")))?;
        match resp.error.clone() {
            Some(e) if !resp.ok => Err(err(e)),
            _ => Ok(resp),
        }
    }

    /// Wait until the shim answers (it binds its socket once the VM's
    /// services are up and the jailer is started).
    pub async fn wait_ready(&self, timeout: Duration, log: &Path) -> Result<Response> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.socket.exists() {
                if let Ok(r) = self.call(&Request::Status).await {
                    return Ok(r);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let tail = tokio::fs::read_to_string(log).await.unwrap_or_default();
                let tail: String = tail
                    .lines()
                    .rev()
                    .take(20)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(err(format!("vmlayer-shim did not start: {tail}")));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

struct State {
    deadline_ms: Option<u64>,
    exposed: BTreeMap<u16, net::Proxy>,
    stop: bool,
}

/// Entry point of the `vmlayer-shim` binary.
pub fn main() -> i32 {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VMLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init();
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: vmlayer-shim SPEC.json");
        return 2;
    };
    let spec: ShimSpec = match std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vmlayer-shim: {path}: {e}");
            return 2;
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("vmlayer-shim: runtime: {e}");
            return 1;
        }
    };
    match rt.block_on(run(spec)) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "shim failed");
            1
        }
    }
}

async fn run(spec: ShimSpec) -> Result<()> {
    tracing::info!(id = %spec.id, "shim starting");
    let _ = std::fs::remove_file(&spec.control);
    let _ = std::fs::remove_file(&spec.exit_file);

    // Lazy volumes: block device first, so the VM can open it at boot.
    #[cfg(target_os = "linux")]
    let mut lazy: Vec<(Arc<vmlayer_blob::LazyImage>, crate::nbd::NbdExport)> = Vec::new();
    #[cfg(target_os = "linux")]
    for l in &spec.lazy {
        let remote = vmlayer_blob::Remote::open(&l.store_url, &l.store_options)
            .map_err(|e| err(format!("blob store: {e}")))?;
        let manifest = remote
            .manifest(&l.snapshot)
            .await
            .map_err(|e| err(e.to_string()))?;
        let img = vmlayer_blob::LazyImage::open(remote, &manifest, &l.image)
            .map_err(|e| err(e.to_string()))?;
        let export = tokio::task::spawn_blocking({
            let img = img.clone();
            move || crate::nbd::NbdExport::attach(img)
        })
        .await
        .map_err(|e| err(e.to_string()))??;
        make_device_node(&export.device, &l.node, spec.jail_uid, spec.jail_gid)?;
        let background = img.clone();
        tokio::spawn(async move {
            if let Err(e) = background.hydrate(16).await {
                if !matches!(e, vmlayer_blob::Error::Closed) {
                    tracing::warn!(error = %e, "background download stopped");
                }
            }
        });
        lazy.push((img, export));
    }
    #[cfg(not(target_os = "linux"))]
    if !spec.lazy.is_empty() {
        return Err(err("lazy volumes need Linux"));
    }

    #[cfg(target_os = "linux")]
    let _uffd = match &spec.uffd {
        Some(u) => Some(crate::uffd::UffdServer::listen(
            &u.socket,
            &u.mem,
            spec.jail_uid,
            spec.jail_gid,
        )?),
        None => None,
    };
    let _dns = match &spec.dns {
        Some(d) => {
            let sink = Arc::new(crate::sandbox::NftSink(d.netns.clone()));
            let resolver = crate::dns::Resolver::new(d.egress.clone(), d.upstreams.clone(), sink);
            Some(crate::dns::DnsProxy::start(d.listen, resolver).await?)
        }
        None => None,
    };

    let console = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.console)?;
    let mut child = tokio::process::Command::new(&spec.jailer[0])
        .args(&spec.jailer[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(console.try_clone()?))
        .stderr(std::process::Stdio::from(console))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| err(format!("spawning jailer: {e}")))?;
    let pid = child.id().unwrap_or(0);
    tokio::fs::write(&spec.pid_file, pid.to_string()).await?;

    let state = Arc::new(Mutex::new(State {
        deadline_ms: spec.deadline_ms,
        exposed: BTreeMap::new(),
        stop: false,
    }));
    let listener = UnixListener::bind(&spec.control)?;
    #[cfg(target_os = "linux")]
    let images: Vec<Arc<vmlayer_blob::LazyImage>> = lazy.iter().map(|(i, _)| i.clone()).collect();
    #[cfg(not(target_os = "linux"))]
    let images: Vec<Arc<vmlayer_blob::LazyImage>> = Vec::new();
    tracing::info!(id = %spec.id, pid, "VM started");

    let mut fenced = false;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let (state, target, images) = (state.clone(), spec.expose_target, images.clone());
                    tokio::spawn(async move {
                        let _ = serve(stream, state, target, pid, images).await;
                    });
                }
            }
            _ = tick.tick() => {
                let s = state.lock().await;
                if s.stop {
                    drop(s);
                    let _ = child.start_kill();
                } else if s.deadline_ms.is_some_and(|d| now_ms() >= d) {
                    drop(s);
                    tracing::error!(id = %spec.id, "fencing deadline passed; killing the VM");
                    fenced = true;
                    let _ = child.start_kill();
                }
            }
        }
    };
    let exit = Exit {
        code: status.and_then(|s| s.code()),
        signal: status.and_then(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.signal()
        }),
        fenced,
    };
    tracing::info!(id = %spec.id, ?exit, "VM exited");
    let _ = std::fs::write(
        &spec.exit_file,
        serde_json::to_vec(&exit).unwrap_or_default(),
    );
    let _ = std::fs::remove_file(&spec.control);
    state.lock().await.exposed.clear();
    // Disconnect devices, then let the images go: whoever runs next (the
    // daemon) resumes downloading from the persisted chunk markers.
    #[cfg(target_os = "linux")]
    for (img, export) in lazy {
        drop(export);
        let _ = img.flush().await;
        img.close().await;
    }
    Ok(())
}

async fn serve(
    stream: UnixStream,
    state: Arc<Mutex<State>>,
    target: Ipv4Addr,
    pid: u32,
    images: Vec<Arc<vmlayer_blob::LazyImage>>,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut line = String::new();
    BufReader::new(rd).read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Err(e) => Response {
            error: Some(format!("bad request: {e}")),
            ..Default::default()
        },
        Ok(req) => handle(req, &state, target, pid, &images)
            .await
            .unwrap_or_else(|e| Response {
                error: Some(e.to_string()),
                ..Default::default()
            }),
    };
    let mut out = serde_json::to_vec(&resp).map_err(|e| err(e.to_string()))?;
    out.push(b'\n');
    wr.write_all(&out).await?;
    Ok(())
}

async fn handle(
    req: Request,
    state: &Mutex<State>,
    target: Ipv4Addr,
    pid: u32,
    images: &[Arc<vmlayer_blob::LazyImage>],
) -> Result<Response> {
    let mut s = state.lock().await;
    let mut resp = Response {
        ok: true,
        pid: Some(pid),
        ..Default::default()
    };
    match req {
        Request::Status => {}
        Request::Extend { deadline_ms } => s.deadline_ms = deadline_ms,
        Request::Expose { port } => {
            if let std::collections::btree_map::Entry::Vacant(slot) = s.exposed.entry(port) {
                slot.insert(net::proxy(target, port).await?);
            }
            resp.host_port = s.exposed.get(&port).map(|p| p.port);
        }
        Request::Unexpose { port } => {
            s.exposed.remove(&port);
        }
        Request::Stop => s.stop = true,
    }
    resp.deadline_ms = s.deadline_ms;
    resp.exposed = s.exposed.iter().map(|(g, p)| (*g, p.port)).collect();
    resp.lazy_missing = images.iter().map(|i| i.missing()).collect();
    Ok(resp)
}

#[cfg(target_os = "linux")]
fn make_device_node(device: &Path, node: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(device)?.rdev();
    let _ = std::fs::remove_file(node);
    nix::sys::stat::mknod(
        node,
        nix::sys::stat::SFlag::S_IFBLK,
        nix::sys::stat::Mode::from_bits_truncate(0o600),
        rdev,
    )
    .map_err(|e| err(format!("device node {}: {e}", node.display())))?;
    std::os::unix::fs::chown(node, Some(uid), Some(gid))?;
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: Error) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips() {
        for req in [
            Request::Status,
            Request::Extend {
                deadline_ms: Some(5),
            },
            Request::Expose { port: 8080 },
            Request::Stop,
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
        assert_eq!(
            serde_json::to_string(&Request::Stop).unwrap(),
            r#"{"op":"stop"}"#
        );
    }
}
