//! Listener setup: unix socket (default) or TCP with bearer tokens and,
//! optionally, TLS or mutual TLS.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tonic::transport::Server;

use crate::manager::Daemon;

pub enum Listen {
    Unix(std::path::PathBuf),
    Tcp(std::net::SocketAddr),
}

pub fn parse_listen(s: &str) -> anyhow::Result<Listen> {
    if let Some(p) = s.strip_prefix("unix://") {
        return Ok(Listen::Unix(p.into()));
    }
    if let Some(a) = s.strip_prefix("tcp://") {
        return Ok(Listen::Tcp(
            a.parse()
                .with_context(|| format!("bad tcp address {a:?}"))?,
        ));
    }
    if s.starts_with('/') {
        return Ok(Listen::Unix(s.into()));
    }
    anyhow::bail!("listen must be unix:///path or tcp://host:port, got {s:?}")
}

/// How TCP callers authenticate (decision 0020).
#[derive(Clone, Debug, Default)]
pub struct Security {
    /// The single administrator token (`--token`).
    pub token: Option<String>,
    pub auth: crate::auth::AuthConfig,
    pub tls: crate::auth::TlsConfig,
}

fn tls_config(tls: &crate::auth::TlsConfig) -> anyhow::Result<tonic::transport::ServerTlsConfig> {
    use tonic::transport::{Certificate, Identity, ServerTlsConfig};
    let read = |p: &Path| std::fs::read(p).with_context(|| format!("reading {}", p.display()));
    let (Some(cert), Some(key)) = (&tls.cert, &tls.key) else {
        anyhow::bail!("[tls] needs both cert and key");
    };
    let mut cfg = ServerTlsConfig::new().identity(Identity::from_pem(read(cert)?, read(key)?));
    if let Some(ca) = &tls.client_ca {
        cfg = cfg.client_ca_root(Certificate::from_pem(read(ca)?));
    }
    Ok(cfg)
}

pub async fn serve(
    daemon: Arc<Daemon>,
    listen: Listen,
    security: Security,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let metrics = daemon.metrics.clone();
    let routes =
        tonic::service::Routes::from(crate::grpc::router(daemon).into_axum_router().layer(
            axum::middleware::from_fn_with_state(metrics, crate::metrics::trace),
        ));
    match listen {
        Listen::Unix(path) => {
            prepare_socket_path(&path)?;
            let listener = tokio::net::UnixListener::bind(&path)
                .with_context(|| format!("binding {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
            }
            tracing::info!(socket = %path.display(), "listening");
            let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
            let result = Server::builder()
                .add_routes(routes)
                .serve_with_incoming_shutdown(incoming, shutdown)
                .await;
            let _ = std::fs::remove_file(&path);
            result.context("serving")
        }
        Listen::Tcp(addr) => {
            let tokens = crate::auth::Tokens::new(security.token.as_deref(), &security.auth)?;
            anyhow::ensure!(
                !tokens.is_empty(),
                "tcp listeners need a token (--token or [auth] tokens)"
            );
            let mut builder = Server::builder();
            let transport = if security.tls.enabled() {
                builder = builder
                    .tls_config(tls_config(&security.tls)?)
                    .context("configuring TLS")?;
                if security.tls.client_ca.is_some() {
                    "mutual TLS"
                } else {
                    "TLS"
                }
            } else {
                if !addr.ip().is_loopback() {
                    tracing::warn!(
                        %addr,
                        "serving plaintext on a non-loopback address: tokens and data cross \
                         the network unencrypted; set [tls] cert and key"
                    );
                }
                "plaintext"
            };
            tracing::info!(%addr, transport, "listening (tcp, bearer token required)");
            let routes = routes
                .into_axum_router()
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(tokens),
                    crate::auth::authenticate,
                ));
            builder
                .add_routes(tonic::service::Routes::from(routes))
                .serve_with_shutdown(addr, shutdown)
                .await
                .context("serving")
        }
    }
}

fn prepare_socket_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        // Refuse to steal a socket another daemon is serving.
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!(
                "another vmlayerd is already listening on {}",
                path.display()
            );
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}
