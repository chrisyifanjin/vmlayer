//! Integration tests against a real Docker engine. They run only when
//! `VMLAYER_DOCKER_TESTS=1` so `cargo test` stays green on machines without
//! Docker; CI sets it.

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vmlayer_backend_docker::{Config, DockerBackend};
use vmlayer_core::*;

const IMAGE: &str = "alpine:3.20";
const PY_IMAGE: &str = "python:3.12-alpine";

fn enabled() -> bool {
    std::env::var("VMLAYER_DOCKER_TESTS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn backend() -> DockerBackend {
    DockerBackend::connect(Config::default()).expect("docker client")
}

fn spec(id: &str, entrypoint: Option<Vec<&str>>) -> SandboxSpec {
    spec_with(IMAGE, id, entrypoint)
}

fn spec_with(image: &str, id: &str, entrypoint: Option<Vec<&str>>) -> SandboxSpec {
    SandboxSpec {
        id: SandboxId(format!("t-{id}-{}", uuid::Uuid::now_v7().simple())),
        rootfs: Rootfs::EngineImage {
            digest: image.into(),
        },
        mounts: Vec::new(),
        vcpus: 1,
        memory_mib: 128,
        disk_mib: None,
        env: BTreeMap::from([("GREETING".to_string(), "hi".to_string())]),
        workdir: "/work".into(),
        entrypoint: entrypoint.map(|argv| ExecRequest {
            argv: argv.into_iter().map(String::from).collect(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: None,
            pty: None,
            user: None,
        }),
        network: NetworkMode::EgressAndExpose,
        egress: None,
        limits: Default::default(),
        ttl: None,
        labels: BTreeMap::from([("suite".to_string(), "live".to_string())]),
    }
}

fn req(argv: &[&str]) -> ExecRequest {
    ExecRequest {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: BTreeMap::new(),
        timeout: None,
        pty: None,
        user: None,
    }
}

async fn run(sb: &dyn SandboxHandle, argv: &[&str]) -> (ExitStatus, String, String) {
    let mut p = sb.exec(req(argv)).await.unwrap();
    let mut out = String::new();
    let mut err = String::new();
    {
        let mut s = p.output();
        while let Some(c) = s.next().await {
            let c = c.unwrap();
            match c.stream {
                OutputStream::Stdout => out.push_str(&String::from_utf8_lossy(&c.data)),
                OutputStream::Stderr => err.push_str(&String::from_utf8_lossy(&c.data)),
            }
        }
    }
    (p.wait().await.unwrap(), out, err)
}

#[tokio::test]
async fn exec_files_and_lifecycle() {
    if !enabled() {
        return;
    }
    let be = backend();
    be.probe().await.unwrap();
    let sb = be.create(spec("basic", None)).await.unwrap();
    assert_eq!(sb.state().await.unwrap(), SandboxState::Running);

    // exec: stdout, stderr, exit code, env, workdir
    let (st, out, err) = run(
        sb.as_ref(),
        &["sh", "-c", "echo $GREETING; pwd; echo oops >&2; exit 3"],
    )
    .await;
    assert_eq!(st.code, Some(3));
    assert_eq!(out, "hi\n/work\n");
    assert_eq!(err, "oops\n");

    // stdin round-trip
    let mut p = sb.exec(req(&["cat"])).await.unwrap();
    let mut stdin = p.stdin().unwrap();
    stdin.write_all(b"ping").await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let mut got = Vec::new();
    {
        let mut s = p.output();
        while let Some(c) = s.next().await {
            got.extend_from_slice(&c.unwrap().data);
        }
    }
    assert_eq!(got, b"ping");
    assert_eq!(p.wait().await.unwrap().code, Some(0));

    // files
    sb.write_file("/work/sub/a.txt", Bytes::from_static(b"hello"), 0o600)
        .await
        .unwrap();
    assert_eq!(
        sb.read_file("/work/sub/a.txt").await.unwrap(),
        Bytes::from_static(b"hello")
    );
    sb.mkdir("/work/sub/dir").await.unwrap();
    let entries = sb.list_dir("/work/sub").await.unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, vec!["/work/sub/a.txt", "/work/sub/dir"]);
    assert_eq!(entries[0].mode, 0o600);
    assert!(entries[1].is_dir);
    assert!(sb.read_file("/work/nope").await.is_err());
    sb.remove("/work/sub", true).await.unwrap();
    assert!(sb.list_dir("/work/sub").await.is_err());

    // the default pids limit is applied (fork bombs stay inside the sandbox)
    let (_, pids_max, _) = run(sb.as_ref(), &["cat", "/sys/fs/cgroup/pids.max"]).await;
    assert_eq!(pids_max.trim(), "4096");

    // kill a long-running process
    let mut p = sb.exec(req(&["sleep", "30"])).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sb.processes().await.unwrap().len(), 1);
    p.kill(9).await.unwrap();
    let st = tokio::time::timeout(Duration::from_secs(5), p.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(st.signal, Some(9));

    // pause / resume
    sb.pause().await.unwrap();
    assert_eq!(sb.state().await.unwrap(), SandboxState::Paused);
    sb.resume().await.unwrap();
    assert_eq!(sb.state().await.unwrap(), SandboxState::Running);

    // listed by the backend
    let listed = be.list().await.unwrap();
    assert!(listed
        .iter()
        .any(|s| &s.id == sb.id() && s.labels.get("suite").map(String::as_str) == Some("live")));

    sb.kill().await.unwrap();
    sb.kill().await.unwrap(); // idempotent
    assert!(matches!(sb.state().await, Err(Error::SandboxNotFound(_))));
}

#[tokio::test]
async fn entrypoint_expose_and_snapshot() {
    if !enabled() {
        return;
    }
    let be = backend();
    let sb = be
        .create(spec_with(
            PY_IMAGE,
            "srv",
            Some(vec!["sh", "-c", "mkdir -p /srv && echo served > /srv/index.html && cd /srv && exec python3 -m http.server 8080"]),
        ))
        .await
        .unwrap();
    sb.wait_for_port(8080, Duration::from_secs(10))
        .await
        .unwrap();
    let procs = sb.processes().await.unwrap();
    assert!(procs.iter().any(|p| p.pid == 1 && p.is_entrypoint));

    let ep = sb.expose(8080).await.unwrap();
    let mut conn = tokio::net::TcpStream::connect((ep.host.as_str(), ep.port))
        .await
        .unwrap();
    conn.write_all(b"GET /index.html HTTP/1.0\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut resp = String::new();
    conn.read_to_string(&mut resp).await.unwrap();
    assert!(resp.contains("served"), "response: {resp}");
    assert_eq!(sb.expose(8080).await.unwrap(), ep, "expose is idempotent");
    sb.unexpose(8080).await.unwrap();

    // snapshot (filesystem only) and restore into a new sandbox
    sb.write_file("/work/state.txt", Bytes::from_static(b"v1"), 0o644)
        .await
        .unwrap();
    let snap = sb.snapshot(SnapshotOptions::default()).await.unwrap();
    assert!(!snap.includes_memory);
    assert_eq!(sb.state().await.unwrap(), SandboxState::Running);
    sb.write_file("/work/state.txt", Bytes::from_static(b"v2"), 0o644)
        .await
        .unwrap();

    let restored = be
        .restore(
            &snap,
            RestoreOptions {
                id: Some(SandboxId(format!(
                    "t-restored-{}",
                    uuid::Uuid::now_v7().simple()
                ))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored.read_file("/work/state.txt").await.unwrap(),
        Bytes::from_static(b"v1")
    );
    // the entrypoint runs again in the restored sandbox
    restored
        .wait_for_port(8080, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(!be.capabilities().fork);

    restored.kill().await.unwrap();
    sb.kill().await.unwrap();
    be.delete_snapshot(&snap).await.unwrap();
}

#[tokio::test]
async fn pty_round_trip() {
    if !enabled() {
        return;
    }
    let be = backend();
    let sb = be.create(spec("pty", None)).await.unwrap();
    let mut r = req(&["sh"]);
    r.pty = Some(PtySize {
        cols: 100,
        rows: 30,
    });
    let mut pty = sb.pty(r).await.unwrap();
    pty.write(Bytes::from_static(b"stty size; echo marker-$((1+1))\n"))
        .await
        .unwrap();
    let mut seen = String::new();
    let mut out = pty.output();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen.contains("marker-2") && tokio::time::Instant::now() < deadline {
        if let Ok(Some(c)) = tokio::time::timeout(Duration::from_secs(2), out.next()).await {
            seen.push_str(&String::from_utf8_lossy(&c.unwrap()));
        }
    }
    assert!(seen.contains("30 100"), "pty size not applied: {seen:?}");
    assert!(seen.contains("marker-2"));
    drop(out);
    pty.close().await.unwrap();
    sb.kill().await.unwrap();
}

#[tokio::test]
async fn image_resolver_reads_config() {
    if !enabled() {
        return;
    }
    let be = backend();
    let img = be.resolve("python:3.12-alpine").await.unwrap();
    assert!(matches!(img.rootfs, Rootfs::EngineImage { .. }));
    assert_eq!(
        img.entrypoint.as_deref(),
        Some(&["python3".to_string()][..])
    );
    assert!(img.env.contains_key("PATH"));
}
