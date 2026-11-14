//! Docker backend: runs each sandbox as a container. Meant for laptops and
//! CI, where there is no KVM. Same API as the microVM backends; what it
//! cannot do (memory snapshots, fork, hot attach) it refuses with
//! [`Error::Unsupported`].
//!
//! Design notes:
//! - The container's PID 1 is a keep-alive shell loop, so the sandbox stays
//!   up regardless of what the entrypoint does. The entrypoint (image `CMD`
//!   or an explicit argv) runs as an exec and is tracked as process 1.
//! - Processes are Docker execs. Docker cannot signal an exec, so `kill`
//!   looks up the in-container pid and runs `kill` inside the container.
//! - Files use the engine's archive API for read/write and a `stat` exec for
//!   metadata. Images without `sh`, `find` and `stat` (distroless) are not
//!   supported by this backend.
//! - Snapshots are `docker commit`; filesystem only.
//! - `expose` starts a `socat` helper container with a published host port
//!   forwarding to the sandbox's bridge address, because Docker cannot add
//!   port mappings to a running container.

mod connect;
mod expose;
mod files;
mod process;
mod sandbox;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
};
use bollard::Docker;
use futures::StreamExt;
use vmlayer_core::{
    Backend, Capabilities, Error, ImageResolver, ResolvedImage, RestoreOptions, Result, Rootfs,
    SandboxHandle, SandboxId, SandboxInfo, SandboxSpec, SandboxState, SnapshotRef, VolumeSource,
};

pub use sandbox::DockerSandbox;

pub const BACKEND_NAME: &str = "docker";
pub const LABEL_MANAGED: &str = "vmlayer.managed";
pub const LABEL_SANDBOX_ID: &str = "vmlayer.sandbox_id";
pub const LABEL_ROLE: &str = "vmlayer.role";

/// Tunables. Defaults are fine for development.
#[derive(Clone, Debug)]
pub struct Config {
    /// Image used for port-forwarding helper containers.
    pub socat_image: String,
    /// Docker network to attach sandboxes to (`bridge` by default).
    pub network: String,
    /// Engine address (`unix:///path` or `tcp://host:port`). `None` means
    /// `DOCKER_HOST`, then the usual socket locations.
    pub host: Option<String>,
    /// Small image used by `verify_shared_path`.
    pub probe_image: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socat_image: "alpine/socat:1.8.0.0".into(),
            network: "bridge".into(),
            host: None,
            probe_image: "alpine:3.20".into(),
        }
    }
}

pub struct DockerBackend {
    docker: Arc<Docker>,
    config: Config,
}

impl DockerBackend {
    /// Connect using the usual environment (`DOCKER_HOST` or the local socket).
    pub fn connect(config: Config) -> Result<Self> {
        let docker = connect::connect(config.host.as_deref())?;
        Ok(Self {
            docker: Arc::new(docker),
            config,
        })
    }

    pub fn with_client(docker: Docker, config: Config) -> Self {
        Self {
            docker: Arc::new(docker),
            config,
        }
    }

    /// Check that a host directory bind-mounted into a container shows the
    /// host's files. On macOS and Windows the engine runs in a VM that shares
    /// only some paths (Docker Desktop and colima share `$HOME`); a volume
    /// directory outside them silently becomes an empty directory inside the
    /// VM and snapshots would copy nothing.
    pub async fn verify_shared_path(&self, path: &std::path::Path) -> Result<()> {
        use bollard::query_parameters::{
            LogsOptionsBuilder, RemoveContainerOptionsBuilder, WaitContainerOptionsBuilder,
        };
        let image = &self.config.probe_image;
        self.ensure_image(image).await?;
        let token = uuid::Uuid::now_v7().simple().to_string();
        let probe_dir = path.join(".vmlayer-probe");
        tokio::fs::create_dir_all(&probe_dir).await?;
        tokio::fs::write(probe_dir.join("token"), &token).await?;
        let body = bollard::models::ContainerCreateBody {
            image: Some(image.clone()),
            cmd: Some(vec!["cat".into(), "/probe/token".into()]),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(vec![format!("{}:/probe:ro", probe_dir.display())]),
                network_mode: Some("none".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(
                None::<bollard::query_parameters::CreateContainerOptions>,
                body,
            )
            .await
            .map_err(engine_err)?;
        let result: Result<String> = async {
            self.docker
                .start_container(&created.id, None)
                .await
                .map_err(engine_err)?;
            let mut wait = self.docker.wait_container(
                &created.id,
                None::<bollard::query_parameters::WaitContainerOptions>,
            );
            while let Some(w) = wait.next().await {
                let _ = w;
            }
            let _ = WaitContainerOptionsBuilder::new;
            let opts = LogsOptionsBuilder::new().stdout(true).stderr(true).build();
            let mut logs = self.docker.logs(&created.id, Some(opts));
            let mut out = String::new();
            while let Some(line) = logs.next().await {
                out.push_str(&line.map_err(engine_err)?.to_string());
            }
            Ok(out)
        }
        .await;
        let _ = self
            .docker
            .remove_container(
                &created.id,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await;
        let _ = tokio::fs::remove_dir_all(&probe_dir).await;
        let out = result?;
        if out.trim() == token {
            Ok(())
        } else {
            Err(Error::Engine {
                backend: BACKEND_NAME,
                message: format!(
                    "{} is not shared with the Docker engine (bind mounts of it are empty inside containers); \
                     move the data dir under a shared path such as your home directory",
                    path.display()
                ),
                source: None,
            })
        }
    }

    /// Engine version string (e.g. `29.5.2`), for `SnapshotRef::engine_version`.
    pub async fn engine_version(&self) -> Result<String> {
        let v = self.docker.version().await.map_err(engine_err)?;
        Ok(v.version.unwrap_or_default())
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        match self.docker.inspect_image(image).await {
            Ok(_) => return Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(e) => return Err(engine_err(e)),
        }
        tracing::info!(image, "pulling image");
        let opts = CreateImageOptionsBuilder::new().from_image(image).build();
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            item.map_err(engine_err)?;
        }
        Ok(())
    }

    async fn create_container(
        &self,
        spec: &SandboxSpec,
        image: &str,
    ) -> Result<Box<dyn SandboxHandle>> {
        let mut binds = Vec::new();
        for m in &spec.mounts {
            match &m.source {
                VolumeSource::Dir { path } => {
                    let mode = if m.read_only { "ro" } else { "rw" };
                    binds.push(format!("{}:{}:{}", path.display(), m.guest_path, mode));
                }
                VolumeSource::BlockImage { .. } => {
                    return Err(Error::Unsupported {
                        backend: BACKEND_NAME,
                        feature: "block-image volumes",
                    })
                }
            }
        }
        let network_mode = match spec.network {
            vmlayer_core::NetworkMode::None => "none".to_string(),
            _ => self.config.network.clone(),
        };
        let mut labels = HashMap::new();
        labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
        labels.insert(LABEL_SANDBOX_ID.to_string(), spec.id.0.clone());
        labels.insert(LABEL_ROLE.to_string(), "sandbox".to_string());
        for (k, v) in &spec.labels {
            labels.insert(format!("vmlayer.user.{k}"), v.clone());
        }
        let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let host_config = bollard::models::HostConfig {
            binds: Some(binds),
            memory: Some(i64::from(spec.memory_mib) * 1024 * 1024),
            nano_cpus: Some(i64::from(spec.vcpus) * 1_000_000_000),
            network_mode: Some(network_mode),
            init: Some(true),
            ..Default::default()
        };
        let body = bollard::models::ContainerCreateBody {
            image: Some(image.to_string()),
            // Keep-alive PID 1; the entrypoint runs as an exec (see module docs).
            entrypoint: Some(vec!["/bin/sh".into(), "-c".into()]),
            cmd: Some(vec!["while :; do sleep 2147483647; done".into()]),
            env: Some(env),
            working_dir: Some(spec.workdir.clone()),
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new()
            .name(&format!("vmlayer-{}", spec.id))
            .build();
        let created = self
            .docker
            .create_container(Some(opts), body)
            .await
            .map_err(engine_err)?;
        self.docker
            .start_container(&created.id, None)
            .await
            .map_err(engine_err)?;
        let handle = DockerSandbox::new(
            self.docker.clone(),
            self.config.clone(),
            spec.id.clone(),
            created.id,
        );
        handle.remember_spec(spec.clone()).await;
        if let Some(entry) = &spec.entrypoint {
            handle.start_entrypoint(entry.clone()).await?;
        }
        Ok(Box::new(handle))
    }
}

#[async_trait]
impl Backend for DockerBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CONTAINER
    }

    async fn probe(&self) -> Result<()> {
        self.docker.ping().await.map(|_| ()).map_err(engine_err)
    }

    async fn create(&self, spec: SandboxSpec) -> Result<Box<dyn SandboxHandle>> {
        let image = match &spec.rootfs {
            Rootfs::EngineImage { digest } => digest.clone(),
            _ => {
                return Err(Error::Unsupported {
                    backend: BACKEND_NAME,
                    feature: "non-engine rootfs",
                })
            }
        };
        self.ensure_image(&image).await?;
        self.create_container(&spec, &image).await
    }

    async fn restore(
        &self,
        snapshot: &SnapshotRef,
        opts: RestoreOptions,
    ) -> Result<Box<dyn SandboxHandle>> {
        if snapshot.backend != BACKEND_NAME {
            return Err(Error::SnapshotIncompatible {
                id: snapshot.id.clone(),
                reason: format!("taken by backend {}", snapshot.backend),
            });
        }
        let mut spec = snapshot.spec.clone();
        if let Some(id) = opts.id {
            spec.id = id;
        }
        spec.ttl = opts.ttl.or(spec.ttl);
        spec.mounts = opts.mounts;
        spec.labels.extend(opts.labels);
        // The committed image already contains the entrypoint's effects; we
        // still start it again so a server-shaped sandbox comes back serving.
        self.create_container(&spec, &snapshot.engine_ref).await
    }

    async fn attach(&self, id: &SandboxId) -> Result<Box<dyn SandboxHandle>> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{LABEL_SANDBOX_ID}={id}")],
        );
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let found = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(engine_err)?;
        let c = found
            .into_iter()
            .find(|c| {
                c.labels
                    .as_ref()
                    .and_then(|l| l.get(LABEL_ROLE))
                    .map(String::as_str)
                    == Some("sandbox")
            })
            .ok_or_else(|| Error::SandboxNotFound(id.clone()))?;
        let cid = c.id.ok_or_else(|| Error::SandboxNotFound(id.clone()))?;
        Ok(Box::new(DockerSandbox::new(
            self.docker.clone(),
            self.config.clone(),
            id.clone(),
            cid,
        )))
    }

    async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_ROLE}=sandbox")]);
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let found = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(engine_err)?;
        Ok(found
            .into_iter()
            .filter_map(|c| {
                let labels = c.labels.unwrap_or_default();
                let id = labels.get(LABEL_SANDBOX_ID)?.clone();
                let user_labels: BTreeMap<String, String> = labels
                    .iter()
                    .filter_map(|(k, v)| {
                        k.strip_prefix("vmlayer.user.")
                            .map(|k| (k.to_string(), v.clone()))
                    })
                    .collect();
                Some(SandboxInfo {
                    id: SandboxId(id),
                    state: state_from_status(
                        c.state
                            .as_ref()
                            .map(|s| format!("{s:?}").to_lowercase())
                            .as_deref(),
                    ),
                    backend: BACKEND_NAME.to_string(),
                    created_at: std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(c.created.unwrap_or(0).max(0) as u64),
                    labels: user_labels,
                })
            })
            .collect())
    }

    async fn delete_snapshot(&self, snapshot: &SnapshotRef) -> Result<()> {
        match self
            .docker
            .remove_image(
                &snapshot.engine_ref,
                None::<bollard::query_parameters::RemoveImageOptions>,
                None,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(engine_err(e)),
        }
    }
}

#[async_trait]
impl ImageResolver for DockerBackend {
    async fn resolve(&self, image: &str) -> Result<ResolvedImage> {
        self.ensure_image(image).await?;
        let info = self.docker.inspect_image(image).await.map_err(engine_err)?;
        let cfg = info.config.unwrap_or_default();
        let mut entrypoint = cfg.entrypoint.unwrap_or_default();
        entrypoint.extend(cfg.cmd.unwrap_or_default());
        let env = cfg
            .env
            .unwrap_or_default()
            .into_iter()
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        Ok(ResolvedImage {
            rootfs: Rootfs::EngineImage {
                digest: image.to_string(),
            },
            entrypoint: if entrypoint.is_empty() {
                None
            } else {
                Some(entrypoint)
            },
            workdir: cfg.working_dir.filter(|w| !w.is_empty()),
            env,
        })
    }
}

pub(crate) fn engine_err(e: bollard::errors::Error) -> Error {
    Error::Engine {
        backend: BACKEND_NAME,
        message: e.to_string(),
        source: Some(Box::new(e)),
    }
}

pub(crate) fn state_from_status(status: Option<&str>) -> SandboxState {
    match status {
        Some("running") => SandboxState::Running,
        Some("paused") => SandboxState::Paused,
        Some("created") | Some("restarting") => SandboxState::Creating,
        Some("exited") | Some("removing") | Some("dead") => SandboxState::Stopped,
        _ => SandboxState::Failed,
    }
}
