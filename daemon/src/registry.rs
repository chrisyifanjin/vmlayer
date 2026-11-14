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
