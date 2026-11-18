//! The set of backends this host can use, probed once at startup.

use std::collections::BTreeMap;
use std::sync::Arc;

use vmlayer_core::{Backend, Capabilities, Error, ImageResolver, Result};

use crate::config::Config;

pub struct BackendEntry {
    pub backend: Arc<dyn Backend>,
    pub resolver: Arc<dyn ImageResolver>,
    pub engine_version: String,
    /// `None` when usable; otherwise why `probe` failed.
    pub unavailable: Option<String>,
}

pub struct Registry {
    entries: BTreeMap<String, BackendEntry>,
    /// Preference order for `auto`.
    order: Vec<String>,
    default: Option<String>,
}

impl Registry {
    pub async fn from_config(cfg: &Config) -> Registry {
        let mut entries = BTreeMap::new();
        let order = vec![
            "firecracker".to_string(),
            "libkrun".to_string(),
            "docker".to_string(),
        ];
        if cfg.docker.enabled {
            let dc = vmlayer_backend_docker::Config {
                host: cfg.docker.host.clone(),
                network: cfg.docker.network.clone(),
                socat_image: cfg.docker.socat_image.clone(),
                probe_image: cfg.docker.probe_image.clone(),
                pids_limit: cfg.docker.pids_limit,
                block_metadata: cfg.docker.block_metadata,
            };
            match vmlayer_backend_docker::DockerBackend::connect(dc) {
                Ok(be) => {
                    let be = Arc::new(be);
                    let mut unavailable = be.probe().await.err().map(|e| e.to_string());
                    if unavailable.is_none() && cfg.docker.verify_mounts {
                        if let Err(e) = be.verify_shared_path(&cfg.data_dir).await {
                            unavailable = Some(e.to_string());
                        }
                    }
                    let version = if unavailable.is_none() {
                        be.engine_version().await.unwrap_or_default()
                    } else {
                        String::new()
                    };
                    if let Some(reason) = &unavailable {
                        tracing::warn!(backend = "docker", %reason, "backend unavailable");
                    } else {
                        tracing::info!(backend = "docker", %version, "backend ready");
                    }
                    entries.insert(
                        "docker".to_string(),
                        BackendEntry {
                            backend: be.clone(),
                            resolver: be,
                            engine_version: version,
                            unavailable,
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!(backend = "docker", error = %e, "backend unavailable");
                }
            }
        }
        if cfg.firecracker.enabled {
            match firecracker_config(&cfg.firecracker, &cfg.data_dir) {
                Ok(fc) => {
                    let be = Arc::new(vmlayer_backend_firecracker::FirecrackerBackend::new(fc));
                    let unavailable = be.probe().await.err().map(|e| e.to_string());
                    let version = if unavailable.is_none() {
                        vmlayer_backend_firecracker::engine_version(
                            &cfg.firecracker.firecracker_bin,
                        )
                        .await
                        .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    match &unavailable {
                        Some(reason) => {
                            tracing::warn!(backend = "firecracker", %reason, "backend unavailable")
                        }
                        None => tracing::info!(backend = "firecracker", %version, "backend ready"),
                    }
                    let images = Arc::new(crate::images::ImageStore::new(&cfg.data_dir));
                    entries.insert(
                        "firecracker".to_string(),
                        BackendEntry {
                            backend: be,
                            resolver: Arc::new(
                                crate::images::MicrovmImageResolver::new(images)
                                    .with_image_dirs(cfg.firecracker.image_dirs.clone()),
                            ),
                            engine_version: version,
                            unavailable,
                        },
                    );
                }
                Err(e) => tracing::warn!(backend = "firecracker", error = %e, "bad configuration"),
            }
        }
        Registry {
            entries,
            order,
            default: cfg.default_backend.clone(),
        }
    }

    /// Resolve `"auto"` or a name to a usable backend.
    pub fn pick(&self, name: &str) -> Result<(String, &BackendEntry)> {
        let name = if name.is_empty() || name == "auto" {
            match &self.default {
                Some(d) => d.as_str(),
                None => self
                    .order
                    .iter()
                    .find(|n| {
                        self.entries
                            .get(*n)
                            .map(|e| e.unavailable.is_none())
                            .unwrap_or(false)
                    })
                    .map(String::as_str)
                    .ok_or_else(|| {
                        Error::ResourceExhausted("no backend is available on this host".into())
                    })?,
            }
        } else {
            name
        };
        let entry = self.entries.get(name).ok_or_else(|| Error::Unsupported {
            backend: "daemon",
            feature: "unknown backend",
        })?;
        if let Some(reason) = &entry.unavailable {
            return Err(Error::ResourceExhausted(format!(
                "backend {name} unavailable: {reason}"
            )));
        }
        Ok((name.to_string(), entry))
    }

    pub fn get(&self, name: &str) -> Option<&BackendEntry> {
        self.entries.get(name)
    }

    pub fn default_name(&self) -> String {
        self.pick("auto").map(|(n, _)| n).unwrap_or_default()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&String, &BackendEntry)> {
        self.entries.iter()
    }

    pub fn capabilities_of(&self, name: &str) -> Option<Capabilities> {
        self.entries.get(name).map(|e| e.backend.capabilities())
    }
}

fn firecracker_config(
    fc: &crate::config::FirecrackerConfig,
    data_dir: &std::path::Path,
) -> anyhow::Result<vmlayer_backend_firecracker::Config> {
    let (net, prefix) = fc
        .host_subnet
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("host_subnet must be CIDR, got {:?}", fc.host_subnet))?;
    Ok(vmlayer_backend_firecracker::Config {
        firecracker_bin: fc.firecracker_bin.clone(),
        jailer_bin: fc.jailer_bin.clone(),
        kernel: fc.kernel.clone(),
        kernel_args: fc.kernel_args.clone(),
        data_dir: data_dir.join("firecracker"),
        jail_uid: fc.jail_uid,
        jail_gid: fc.jail_gid,
        subnet: vmlayer_backend_firecracker::Subnet {
            network: net.parse()?,
            prefix: prefix.parse()?,
        },
        dns: fc.dns.clone(),
        agent_port: fc.agent_port,
        guest_agent: fc.guest_agent.clone(),
        template_snapshots: fc.template_snapshots,
        template_warmup_argv: fc.template_warmup_argv.clone(),
        template_limit: fc.template_limit,
        shim_bin: fc.shim_bin.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("vmlayer-shim")))
                .unwrap_or_else(|| "vmlayer-shim".into())
        }),
        prefetch_memory: fc.prefetch_memory,
        max_concurrent_boots: fc.max_concurrent_boots,
        boot_timeout: std::time::Duration::from_secs(fc.boot_timeout_secs),
    })
}
