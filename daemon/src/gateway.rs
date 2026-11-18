//! An authenticated HTTPS gateway in front of exposed ports (decision 0021).
//!
//! `expose(port, gateway=true)` publishes a sandbox port as
//! `https://<port>-<sandbox-id>.<domain>` with a random token. The gateway
//! terminates TLS, finds the route from the `Host` header, checks the token
//! and reverse-proxies the request, WebSocket and other upgrades included,
//! to the port's existing host endpoint. Only the token's SHA-256 is stored.
//!
//! A request carries the token as `Authorization: Bearer <token>`, as an
//! `X-Vmlayer-Token` header, or once as `?vmlayer_token=<token>`, which the
//! gateway turns into an HTTP-only cookie for that hostname and redirects,
//! so a browser can open the URL. The token never reaches the sandbox.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use http::{header, HeaderValue, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vmlayer_core::SandboxId;

use crate::manager::Daemon;

const TOKEN_HEADER: &str = "x-vmlayer-token";
const TOKEN_PARAM: &str = "vmlayer_token";
const TOKEN_COOKIE: &str = "vmlayer_token";

/// `[gateway]` in the daemon config. Off unless `listen` and `domain` are set.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Address to serve on, e.g. `0.0.0.0:8443`.
    pub listen: Option<String>,
    /// Ports are served at `<port>-<sandbox-id>.<domain>`; point a wildcard
    /// DNS record `*.<domain>` at the gateway.
    pub domain: Option<String>,
    /// PEM certificate (a wildcard for `*.<domain>`) and key. Without them
    /// the gateway speaks plain HTTP, for use behind a TLS terminator.
    pub cert: Option<std::path::PathBuf>,
    pub key: Option<std::path::PathBuf>,
    /// Port in the URLs handed out, when clients reach the gateway on a
    /// different port than it listens on (behind a load balancer).
    /// Default: the listening port.
    pub public_port: Option<u16>,
}

impl GatewayConfig {
    pub fn enabled(&self) -> bool {
        self.listen.is_some() && self.domain.is_some()
    }

    fn tls(&self) -> bool {
        self.cert.is_some()
    }

    /// The URL of `port` of `sandbox`.
    pub fn url(&self, sandbox: &SandboxId, port: u16) -> String {
        let scheme = if self.tls() { "https" } else { "http" };
        let listen_port = self
            .listen
            .as_deref()
            .and_then(|l| l.parse::<SocketAddr>().ok())
            .map(|a| a.port());
        let public = self.public_port.or(listen_port);
        let default = if self.tls() { 443 } else { 80 };
        let suffix = match public {
            Some(p) if p != default => format!(":{p}"),
            _ => String::new(),
        };
        let domain = self.domain.as_deref().unwrap_or_default();
        format!("{scheme}://{port}-{sandbox}.{domain}{suffix}")
    }
}

/// A fresh token and the hash to store.
pub fn new_token() -> (String, [u8; 32]) {
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let hash = Sha256::digest(token.as_bytes()).into();
    (token, hash)
}

/// The route a hostname names: `<port>-<sandbox-id>.<domain>[:port]`.
fn route_of(host: &str, domain: &str) -> Option<(u16, SandboxId)> {
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let label = host.strip_suffix(domain)?.strip_suffix('.')?;
    if label.contains('.') {
        return None;
    }
    let (port, sandbox) = label.split_once('-')?;
    let port = port.parse::<u16>().ok().filter(|p| *p > 0)?;
    (!sandbox.is_empty()).then(|| (port, SandboxId(sandbox.to_string())))
}

/// Where a request carries its token.
#[derive(Debug, PartialEq, Eq)]
enum Presented {
    Header(String),
    Query(String),
    None,
}

fn presented<B>(req: &Request<B>) -> Presented {
    let h = req.headers();
    if let Some(t) = h.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
        return Presented::Header(t.to_string());
    }
    if let Some(t) = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Presented::Header(t.to_string());
    }
    if let Some(t) = cookie(req) {
        return Presented::Header(t);
    }
    if let Some(t) = req.uri().query().and_then(|q| {
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == TOKEN_PARAM)
            .map(|(_, v)| v.to_string())
    }) {
        return Presented::Query(t);
    }
    Presented::None
}

fn cookie<B>(req: &Request<B>) -> Option<String> {
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == TOKEN_COOKIE)
        .map(|(_, v)| v.to_string())
}

/// Remove the gateway's own credentials before the request reaches the sandbox.
fn strip_credentials<B>(req: &mut Request<B>, token: &str) {
    let h = req.headers_mut();
    h.remove(TOKEN_HEADER);
    if h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {token}"))
    {
        h.remove(header::AUTHORIZATION);
    }
    let kept: Vec<String> = h
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .map(str::trim)
        .filter(|c| !c.is_empty() && !c.starts_with(&format!("{TOKEN_COOKIE}=")))
        .map(String::from)
        .collect();
    h.remove(header::COOKIE);
    if !kept.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&kept.join("; ")) {
            h.insert(header::COOKIE, v);
        }
    }
}

/// The request's path and query without `vmlayer_token`.
fn without_token_param(uri: &http::Uri) -> String {
    let path = uri.path();
    let rest: Vec<&str> = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty() && !kv.starts_with(&format!("{TOKEN_PARAM}=")))
        .collect();
    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

type Body = BoxBody<Bytes, hyper::Error>;

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(
        Full::new(Bytes::from(format!("{msg}\n")))
            .map_err(|never| match never {})
            .boxed(),
    );
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

struct Gateway {
    daemon: Arc<Daemon>,
    domain: String,
    secure: bool,
}

impl Gateway {
    async fn handle(self: Arc<Self>, mut req: Request<Incoming>) -> Response<Body> {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().authority().map(|a| a.to_string()))
            .unwrap_or_default();
        let Some((port, sandbox)) = route_of(&host, &self.domain) else {
            return text(StatusCode::NOT_FOUND, "no such sandbox port");
        };
        // Same answer for "not published" and "wrong token": nothing to probe.
        let denied = || text(StatusCode::UNAUTHORIZED, "missing or invalid token");
        let stored = match self.daemon.store.route(&sandbox, port) {
            Ok(Some(h)) => h,
            Ok(None) => return denied(),
            Err(e) => return text(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        let token = match presented(&req) {
            Presented::None => return denied(),
            Presented::Header(t) | Presented::Query(t) if !matches(&t, &stored) => return denied(),
            Presented::Query(t) => return self.set_cookie(&req, &t),
            Presented::Header(t) => t,
        };
        strip_credentials(&mut req, &token);
        let upstream = match self.daemon.live(&sandbox).await {
            Ok(live) => live.exposed.lock().await.get(&port).cloned(),
            Err(_) => None,
        };
        let Some(ep) = upstream else {
            return text(StatusCode::BAD_GATEWAY, "the port is not exposed any more");
        };
        match proxy(req, &ep.host, ep.port).await {
            Ok(r) => r,
            Err(e) => text(StatusCode::BAD_GATEWAY, &format!("upstream: {e:#}")),
        }
    }

    fn set_cookie(&self, req: &Request<Incoming>, token: &str) -> Response<Body> {
        let mut r = text(StatusCode::FOUND, "authenticated");
        let secure = if self.secure { "; Secure" } else { "" };
        let headers = r.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&without_token_param(req.uri())) {
            headers.insert(header::LOCATION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!(
            "{TOKEN_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax{secure}"
        )) {
            headers.insert(header::SET_COOKIE, v);
        }
        r
    }
}

fn matches(token: &str, stored: &[u8; 32]) -> bool {
    // Comparing digests: timing reveals nothing useful about the token.
    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    hash == *stored
}

/// Forward one request to `host:port`, following a protocol upgrade.
async fn proxy(
    mut req: Request<Incoming>,
    host: &str,
    port: u16,
) -> anyhow::Result<Response<Body>> {
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .context("connecting")?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("handshake")?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let client_upgrade = hyper::upgrade::on(&mut req);
    let mut resp = sender.send_request(req).await.context("sending")?;
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let server_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            let (Ok(client), Ok(server)) = (client_upgrade.await, server_upgrade.await) else {
                return;
            };
            let _ =
                tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(server))
                    .await;
        });
    }
    Ok(resp.map(BodyExt::boxed))
}

fn tls_acceptor(cfg: &GatewayConfig) -> anyhow::Result<Option<tokio_rustls::TlsAcceptor>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let (Some(cert), Some(key)) = (&cfg.cert, &cfg.key) else {
        anyhow::ensure!(cfg.key.is_none(), "[gateway] key without cert");
        return Ok(None);
    };
    let certs = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    let key =
        PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {}", key.display()))?;
    let mut server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(tokio_rustls::TlsAcceptor::from(Arc::new(server))))
}

/// Serve the gateway until the process exits.
pub async fn serve(daemon: Arc<Daemon>, cfg: GatewayConfig) -> anyhow::Result<()> {
    let (Some(listen), Some(domain)) = (cfg.listen.clone(), cfg.domain.clone()) else {
        return Ok(());
    };
    let tls = tls_acceptor(&cfg)?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding the gateway to {listen}"))?;
    tracing::info!(%listen, %domain, tls = tls.is_some(), "port gateway listening");
    let gw = Arc::new(Gateway {
        daemon,
        domain,
        secure: tls.is_some(),
    });
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "gateway accept failed");
                continue;
            }
        };
        let gw = gw.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let gw = gw.clone();
                async move { Ok::<_, std::convert::Infallible>(gw.handle(req).await) }
            });
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let result = match tls {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(s) => {
                        builder
                            .serve_connection_with_upgrades(TokioIo::new(s), service)
                            .await
                    }
                    Err(_) => return,
                },
                None => {
                    builder
                        .serve_connection_with_upgrades(TokioIo::new(stream), service)
                        .await
                }
            };
            if let Err(e) = result {
                tracing::debug!(error = %e, "gateway connection ended");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_name_routes() {
        let d = "sb.example.com";
        assert_eq!(
            route_of("8080-sb-0193abc.sb.example.com", d),
            Some((8080, SandboxId("sb-0193abc".into())))
        );
        assert_eq!(
            route_of("3000-sb-x.sb.example.com:8443", d),
            Some((3000, SandboxId("sb-x".into())))
        );
        assert_eq!(route_of("sb-x.sb.example.com", d), None);
        assert_eq!(route_of("0-sb-x.sb.example.com", d), None);
        assert_eq!(route_of("80-sb-x.evil.com", d), None);
        assert_eq!(route_of("80-sb-x.a.sb.example.com", d), None);
        assert_eq!(route_of("80-sb-xsb.example.com", d), None);
    }

    #[test]
    fn tokens_come_from_headers_cookies_or_the_query() {
        let req = |f: &dyn Fn(http::request::Builder) -> http::request::Builder| {
            f(Request::builder().uri("/p?a=1")).body(()).unwrap()
        };
        assert_eq!(
            presented(&req(&|b| b.header("x-vmlayer-token", "t1"))),
            Presented::Header("t1".into())
        );
        assert_eq!(
            presented(&req(&|b| b.header("authorization", "Bearer t2"))),
            Presented::Header("t2".into())
        );
        assert_eq!(
            presented(&req(&|b| b.header("cookie", "x=1; vmlayer_token=t3"))),
            Presented::Header("t3".into())
        );
        let q = Request::builder()
            .uri("/p?a=1&vmlayer_token=t4")
            .body(())
            .unwrap();
        assert_eq!(presented(&q), Presented::Query("t4".into()));
        assert_eq!(without_token_param(q.uri()), "/p?a=1");
        assert_eq!(presented(&req(&|b| b)), Presented::None);
    }

    #[test]
    fn credentials_do_not_reach_the_sandbox() {
        let mut r = Request::builder()
            .header("x-vmlayer-token", "t")
            .header("authorization", "Bearer t")
            .header("cookie", "a=1; vmlayer_token=t; b=2")
            .body(())
            .unwrap();
        strip_credentials(&mut r, "t");
        assert!(r.headers().get("x-vmlayer-token").is_none());
        assert!(r.headers().get("authorization").is_none());
        assert_eq!(r.headers()["cookie"], "a=1; b=2");
        // The app's own Authorization header passes through.
        let mut own = Request::builder()
            .header("authorization", "Bearer app")
            .body(())
            .unwrap();
        strip_credentials(&mut own, "t");
        assert_eq!(own.headers()["authorization"], "Bearer app");
    }

    #[test]
    fn urls_and_tokens() {
        let cfg = GatewayConfig {
            listen: Some("0.0.0.0:8443".into()),
            domain: Some("sb.example.com".into()),
            cert: Some("c.pem".into()),
            key: Some("k.pem".into()),
            public_port: None,
        };
        let id = SandboxId("sb-1".into());
        assert_eq!(cfg.url(&id, 80), "https://80-sb-1.sb.example.com:8443");
        let behind_lb = GatewayConfig {
            public_port: Some(443),
            ..cfg
        };
        assert_eq!(behind_lb.url(&id, 80), "https://80-sb-1.sb.example.com");
        let (t, h) = new_token();
        assert!(t.len() >= 64 && matches(&t, &h) && !matches("x", &h));
    }
}
