//! Listener setup: unix socket (default) or TCP with a bearer token.

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

fn auth_interceptor(
    token: String,
) -> impl Fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |req: tonic::Request<()>| {
        let ok = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == token)
            .unwrap_or(false);
        if ok {
            Ok(req)
        } else {
            Err(tonic::Status::unauthenticated(
                "missing or invalid bearer token",
            ))
        }
    }
}

pub async fn serve(
    daemon: Arc<Daemon>,
    listen: Listen,
    token: Option<String>,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let routes = crate::grpc::router(daemon);
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
            let token = token.ok_or_else(|| {
                anyhow::anyhow!("a token is required for tcp listeners (--token)")
            })?;
            tracing::info!(%addr, "listening (tcp, bearer token required)");
            let routes = routes
                .into_axum_router()
                .layer(tonic::service::InterceptorLayer::new(auth_interceptor(
                    token,
                )));
            Server::builder()
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
