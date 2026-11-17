//! `vmlayer`: a thin command-line client over the gRPC API, for humans and
//! shell scripts. Everything it does, the SDKs do too.

mod connect;
mod output;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use vmlayer_proto::v1 as pb;

#[derive(Parser, Debug)]
#[command(
    name = "vmlayer",
    version,
    about = "Sandboxes for agents: Firecracker, libkrun or Docker behind one API."
)]
struct Cli {
    /// Daemon endpoint: unix:///path or tcp://host:port. Defaults to the
    /// local socket (VMLAYER_ENDPOINT).
    #[arg(long, global = true, env = "VMLAYER_ENDPOINT")]
    endpoint: Option<String>,
    /// Bearer token for tcp endpoints.
    #[arg(long, global = true, env = "VMLAYER_TOKEN")]
    token: Option<String>,
    /// Print JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Daemon version, backends and their engine versions.
    Info,
    /// Pull an image ahead of time (and build its root filesystem on microVM backends).
    Pull {
        image: String,
        #[arg(long, default_value = "auto")]
        backend: String,
    },
    /// Create a sandbox and print its id.
    Create {
        image: String,
        #[arg(long, default_value_t = 1)]
        cpus: u32,
        #[arg(long, default_value_t = 512)]
        memory: u32,
        /// KEY=VALUE, repeatable.
        #[arg(long = "env", short = 'e')]
        env: Vec<String>,
        /// Named volume to mount as the workspace.
        #[arg(long)]
        workspace: Option<String>,
        /// Hard lifetime in seconds (0 = unbounded).
        #[arg(long, default_value_t = 900)]
        ttl: u64,
        #[arg(long, default_value = "auto")]
        backend: String,
        /// KEY=VALUE, repeatable.
        #[arg(long = "label", short = 'l')]
        labels: Vec<String>,
        /// Override the image entrypoint; `--entrypoint ""` starts nothing.
        #[arg(long)]
        entrypoint: Option<String>,
        #[arg(long)]
        no_network: bool,
    },
    /// Run a command in a fresh sandbox, stream its output, then kill the sandbox.
    Run {
        image: String,
        #[arg(long, default_value_t = 512)]
        memory: u32,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Run a command in an existing sandbox and stream its output.
    Exec {
        sandbox: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// List sandboxes.
    #[command(alias = "ls")]
    List,
    /// Show one sandbox.
    Get {
        sandbox: String,
    },
    /// Destroy sandboxes.
    #[command(alias = "rm")]
    Kill {
        sandboxes: Vec<String>,
    },
    Pause {
        sandbox: String,
    },
    Resume {
        sandbox: String,
    },
    /// Take a sandbox snapshot and print its id.
    Snapshot {
        sandbox: String,
    },
    /// Start a sandbox from a snapshot and print its id.
    Restore {
        snapshot: String,
    },
    /// Expose a guest port and print host:port.
    Expose {
        sandbox: String,
        port: u32,
    },
    /// Block until something listens on a guest port.
    WaitPort {
        sandbox: String,
        port: u32,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Copy a local file into a sandbox: `cp ./a.txt sb-123:/work/a.txt`, or out.
    Cp {
        src: String,
        dst: String,
    },
    /// Volumes.
    #[command(subcommand)]
    Volume(VolumeCmd),
    /// Sandbox snapshots.
    #[command(subcommand)]
    Snapshots(SnapshotsCmd),
}

#[derive(Subcommand, Debug)]
enum VolumeCmd {
    #[command(alias = "ls")]
    List,
    Create {
        name: Option<String>,
        #[arg(long)]
        size: Option<u64>,
    },
    #[command(alias = "rm")]
    Delete { volume: String },
    /// Filesystem snapshot of a volume; prints the snapshot id.
    Snapshot {
        volume: String,
        /// Also store it in the daemon's blob store (S3, Azure Blob, GCS...).
        #[arg(long)]
        durable: bool,
    },
    /// New volume from a volume snapshot (downloaded if only in the blob store).
    FromSnapshot {
        snapshot: String,
        #[arg(long)]
        name: Option<String>,
    },
    #[command(name = "ls-snapshots")]
    ListSnapshots {
        /// List the blob store's snapshots instead of this daemon's.
        #[arg(long)]
        remote: bool,
    },
    /// Delete a volume snapshot, including its blob-store copy.
    #[command(name = "rm-snapshot")]
    DeleteSnapshot {
        snapshot: String,
        /// Free only the local copy of a durable snapshot.
        #[arg(long)]
        local_only: bool,
    },
    /// Delete blob-store chunks that no snapshot references.
    Gc {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SnapshotsCmd {
    #[command(alias = "ls")]
    List,
    #[command(alias = "rm")]
    Delete { snapshot: String },
}

fn kv(pairs: &[String]) -> anyhow::Result<HashMap<String, String>> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .with_context(|| format!("expected KEY=VALUE, got {p:?}"))
        })
        .collect()
}

fn secs(s: u64) -> Option<prost_types::Duration> {
    (s > 0).then(|| prost_types::Duration::try_from(Duration::from_secs(s)).unwrap())
}

fn argv(command: &[String]) -> Vec<String> {
    if command.len() == 1 && command[0].contains(' ') {
        vec!["/bin/sh".into(), "-c".into(), command[0].clone()]
    } else {
        command.to_vec()
    }
}

/// Stream an Exec response to stdout/stderr; returns the exit code.
async fn pump_exec(mut stream: tonic::Streaming<pb::ExecEvent>) -> anyhow::Result<i32> {
    use tokio::io::AsyncWriteExt;
    let mut out = tokio::io::stdout();
    let mut err = tokio::io::stderr();
    let mut code = 1;
    while let Some(ev) = stream.next().await {
        match ev?.event {
            Some(pb::exec_event::Event::Output(c)) => {
                if c.stream == pb::OutputStream::Stderr as i32 {
                    err.write_all(&c.data).await?;
                    err.flush().await?;
                } else {
                    out.write_all(&c.data).await?;
                    out.flush().await?;
                }
            }
            Some(pb::exec_event::Event::Exited(st)) => {
                code = st.code.unwrap_or_else(|| 128 + st.signal.unwrap_or(9));
                if st.timed_out {
                    eprintln!("vmlayer: command timed out");
                }
            }
            Some(pb::exec_event::Event::DroppedBytes(n)) => {
                eprintln!("vmlayer: {n} bytes of output dropped")
            }
            _ => {}
        }
    }
    Ok(code)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut c = connect::connect(cli.endpoint.as_deref(), cli.token.as_deref()).await?;
    let json = cli.json;
    match cli.cmd {
        Cmd::Info => {
            let info = c.system.info(pb::Empty {}).await?.into_inner();
            output::info(&info, json);
        }
        Cmd::Pull { image, backend } => {
            let info = c
                .system
                .pull_image(pb::PullImageRequest { image, backend })
                .await?
                .into_inner();
            if json {
                output::json(&serde_json::json!({
                    "image": info.image, "backend": info.backend, "rootfs_kind": info.rootfs_kind,
                    "rootfs_path": info.rootfs_path, "entrypoint": info.entrypoint, "workdir": info.workdir,
                }));
            } else {
                let place = if info.rootfs_path.is_empty() {
                    String::new()
                } else {
                    format!(" at {}", info.rootfs_path)
                };
                println!(
                    "{} ready for {} ({}{})",
                    info.image, info.backend, info.rootfs_kind, place
                );
            }
        }
        Cmd::Create {
            image,
            cpus,
            memory,
            env,
            workspace,
            ttl,
            backend,
            labels,
            entrypoint,
            no_network,
        } => {
            let mut req = pb::CreateSandboxRequest {
                image,
                vcpus: cpus,
                memory_mib: memory,
                env: kv(&env)?,
                labels: kv(&labels)?,
                ttl: secs(ttl),
                backend,
                network: if no_network {
                    pb::NetworkMode::None
                } else {
                    pb::NetworkMode::EgressAndExpose
                } as i32,
                ..Default::default()
            };
            if let Some(w) = workspace {
                req.workspace = Some(pb::WorkspaceSpec {
                    kind: Some(pb::workspace_spec::Kind::Volume(w)),
                });
            }
            if let Some(e) = entrypoint {
                let kind = if e.is_empty() {
                    pb::entrypoint_spec::Kind::None(pb::Empty {})
                } else {
                    pb::entrypoint_spec::Kind::Argv(pb::Argv { argv: argv(&[e]) })
                };
                req.entrypoint = Some(pb::EntrypointSpec { kind: Some(kind) });
            }
            let sb = c.sandboxes.create(req).await?.into_inner();
            if json {
                output::json(&output::sandbox_json(&sb));
            } else {
                println!("{}", sb.id);
            }
        }
        Cmd::Run {
            image,
            memory,
            command,
        } => {
            let sb = c
                .sandboxes
                .create(pb::CreateSandboxRequest {
                    image,
                    memory_mib: memory,
                    ttl: secs(3600),
                    backend: "auto".into(),
                    network: pb::NetworkMode::EgressAndExpose as i32,
                    entrypoint: Some(pb::EntrypointSpec {
                        kind: Some(pb::entrypoint_spec::Kind::None(pb::Empty {})),
                    }),
                    ..Default::default()
                })
                .await?
                .into_inner();
            let result = async {
                let stream = c
                    .processes
                    .exec(pb::ExecRequest {
                        sandbox_id: sb.id.clone(),
                        argv: argv(&command),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                pump_exec(stream).await
            }
            .await;
            let _ = c.sandboxes.kill(pb::KillSandboxRequest { id: sb.id }).await;
            std::process::exit(result?);
        }
        Cmd::Exec {
            sandbox,
            cwd,
            timeout,
            command,
        } => {
            let stream = c
                .processes
                .exec(pb::ExecRequest {
                    sandbox_id: sandbox,
                    argv: argv(&command),
                    cwd: cwd.unwrap_or_default(),
                    timeout: secs(timeout),
                    ..Default::default()
                })
                .await?
                .into_inner();
            std::process::exit(pump_exec(stream).await?);
        }
        Cmd::List => {
            let r = c
                .sandboxes
                .list(pb::ListSandboxesRequest::default())
                .await?
                .into_inner();
            output::sandboxes(&r.sandboxes, json);
        }
        Cmd::Get { sandbox } => {
            let sb = c
                .sandboxes
                .get(pb::GetSandboxRequest { id: sandbox })
                .await?
                .into_inner();
            output::sandboxes(std::slice::from_ref(&sb), json);
        }
        Cmd::Kill { sandboxes } => {
            for id in sandboxes {
                c.sandboxes
                    .kill(pb::KillSandboxRequest { id: id.clone() })
                    .await?;
                println!("{id}");
            }
        }
        Cmd::Pause { sandbox } => {
            c.sandboxes
                .pause(pb::PauseSandboxRequest { id: sandbox })
                .await?;
        }
        Cmd::Resume { sandbox } => {
            c.sandboxes
                .resume(pb::ResumeSandboxRequest { id: sandbox })
                .await?;
        }
        Cmd::Snapshot { sandbox } => {
            let s = c
                .sandboxes
                .take_snapshot(pb::SnapshotSandboxRequest {
                    id: sandbox,
                    ..Default::default()
                })
                .await?
                .into_inner();
            println!("{}", s.id);
        }
        Cmd::Restore { snapshot } => {
            let sb = c
                .sandboxes
                .restore(pb::RestoreSandboxRequest {
                    snapshot_id: snapshot,
                    ..Default::default()
                })
                .await?
                .into_inner();
            println!("{}", sb.id);
        }
        Cmd::Expose { sandbox, port } => {
            let ep = c
                .sandboxes
                .expose(pb::ExposeRequest {
                    id: sandbox,
                    guest_port: port,
                })
                .await?
                .into_inner();
            println!("{}:{}", ep.host, ep.port);
        }
        Cmd::WaitPort {
            sandbox,
            port,
            timeout,
        } => {
            c.sandboxes
                .wait_for_port(pb::WaitForPortRequest {
                    id: sandbox,
                    port,
                    timeout: secs(timeout),
                })
                .await?;
        }
        Cmd::Cp { src, dst } => connect::copy(&mut c, &src, &dst).await?,
        Cmd::Volume(v) => match v {
            VolumeCmd::List => {
                let r = c
                    .volumes
                    .list(pb::ListVolumesRequest::default())
                    .await?
                    .into_inner();
                output::volumes(&r.volumes, json);
            }
            VolumeCmd::Create { name, size } => {
                let v = c
                    .volumes
                    .create(pb::CreateVolumeRequest {
                        name: name.unwrap_or_default(),
                        size_mib: size,
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                println!("{}", v.id);
            }
            VolumeCmd::Delete { volume } => {
                let v = c
                    .volumes
                    .get(pb::GetVolumeRequest { volume })
                    .await?
                    .into_inner();
                c.volumes
                    .delete(pb::DeleteVolumeRequest { id: v.id })
                    .await?;
            }
            VolumeCmd::Snapshot { volume, durable } => {
                let v = c
                    .volumes
                    .get(pb::GetVolumeRequest { volume })
                    .await?
                    .into_inner();
                let s = c
                    .volumes
                    .take_snapshot(pb::SnapshotVolumeRequest {
                        id: v.id,
                        durable,
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                println!("{}", s.id);
            }
            VolumeCmd::FromSnapshot { snapshot, name } => {
                let v = c
                    .volumes
                    .from_snapshot(pb::VolumeFromSnapshotRequest {
                        snapshot_id: snapshot,
                        name: name.unwrap_or_default(),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                println!("{}", v.id);
            }
            VolumeCmd::ListSnapshots { remote } => {
                let r = c
                    .volumes
                    .list_snapshots(pb::ListVolumeSnapshotsRequest {
                        remote,
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                output::volume_snapshots(&r.snapshots, json);
            }
            VolumeCmd::DeleteSnapshot {
                snapshot,
                local_only,
            } => {
                c.volumes
                    .delete_snapshot(pb::DeleteVolumeSnapshotRequest {
                        id: snapshot,
                        local_only,
                    })
                    .await?;
            }
            VolumeCmd::Gc { dry_run } => {
                let r = c
                    .system
                    .collect_garbage(pb::CollectGarbageRequest { dry_run })
                    .await?
                    .into_inner();
                if json {
                    output::json(&serde_json::json!({
                        "snapshots": r.snapshots, "chunks": r.chunks,
                        "referenced_chunks": r.referenced_chunks, "recent_chunks": r.recent_chunks,
                        "deleted_chunks": r.deleted_chunks, "deleted_bytes": r.deleted_bytes,
                    }));
                } else {
                    let verb = if dry_run { "would delete" } else { "deleted" };
                    println!(
                        "{} snapshots, {} chunks: {} referenced, {} recent; {verb} {} chunks ({} bytes)",
                        r.snapshots,
                        r.chunks,
                        r.referenced_chunks,
                        r.recent_chunks,
                        r.deleted_chunks,
                        r.deleted_bytes
                    );
                }
            }
        },
        Cmd::Snapshots(s) => match s {
            SnapshotsCmd::List => {
                let r = c
                    .system
                    .list_snapshots(pb::ListSnapshotsRequest::default())
                    .await?
                    .into_inner();
                output::snapshots(&r.snapshots, json);
            }
            SnapshotsCmd::Delete { snapshot } => {
                c.system
                    .delete_snapshot(pb::DeleteSnapshotRequest { id: snapshot })
                    .await?;
            }
        },
    }
    Ok(())
}
