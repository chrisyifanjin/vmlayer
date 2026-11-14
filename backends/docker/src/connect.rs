//! Finding the Docker engine. `DOCKER_HOST` wins; otherwise the usual socket
//! locations are probed in order, because Docker Desktop, colima, OrbStack
//! and Rancher Desktop all put it somewhere different.

use std::path::PathBuf;

use bollard::Docker;
use vmlayer_core::{Error, Result};

use crate::BACKEND_NAME;

pub(crate) fn candidate_sockets() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut v = vec![PathBuf::from("/var/run/docker.sock")];
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        v.push(PathBuf::from(dir).join("docker.sock"));
    }
    if let Some(h) = home {
        for rel in [
            ".docker/run/docker.sock",
            ".colima/default/docker.sock",
            ".colima/docker.sock",
            ".orbstack/run/docker.sock",
            ".rd/docker.sock",
        ] {
            v.push(h.join(rel));
        }
    }
    v
}

/// Connect to the engine. `host` (from config) overrides everything; then
/// `DOCKER_HOST`; then the first existing socket from [`candidate_sockets`].
pub(crate) fn connect(host: Option<&str>) -> Result<Docker> {
    let timeout = 120;
    let api = bollard::API_DEFAULT_VERSION;
    if let Some(h) = host {
        return if let Some(path) = h.strip_prefix("unix://") {
            Docker::connect_with_unix(path, timeout, api)
        } else {
            Docker::connect_with_http(h, timeout, api)
        }
        .map_err(crate::engine_err);
    }
    if std::env::var_os("DOCKER_HOST").is_some() {
        return Docker::connect_with_defaults().map_err(crate::engine_err);
    }
    for sock in candidate_sockets() {
        if sock.exists() {
            return Docker::connect_with_unix(&sock.to_string_lossy(), timeout, api)
                .map_err(crate::engine_err);
        }
    }
    Err(Error::Engine {
        backend: BACKEND_NAME,
        message: "no Docker socket found; set DOCKER_HOST or start Docker".into(),
        source: None,
    })
}
