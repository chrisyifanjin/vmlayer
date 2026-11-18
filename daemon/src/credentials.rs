//! A credential proxy on the egress path (decision 0022).
//!
//! Sandboxes never hold real API keys. The operator lists credentials in the
//! daemon config: an upstream (`https://api.anthropic.com`), the header that
//! carries the secret, where the secret comes from, and the environment a
//! sandbox gets. A sandbox created with `credentials = ["anthropic"]` gets a
//! random placeholder and a base URL on this proxy, reached at `vmlayer.host`
//! from inside every sandbox. The proxy finds the placeholder in a request
//! header, checks that its sandbox was granted the credential the path
//! names, swaps in the real secret, and forwards to the upstream over HTTPS,
//! streaming both ways (server-sent events included).
//!
//! No TLS interception and no CA in the guest: clients talk plain HTTP to
//! the proxy through their SDK's base-URL setting. A leaked placeholder is
//! worthless outside the host's sandbox networks and dies with its sandbox.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, BodyStream, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vmlayer_core::SandboxId;

use crate::manager::Daemon;

/// Environment variable that holds a sandbox's placeholder, so a fork or a
/// restore can grant the same credentials to the new sandbox.
pub const TOKEN_ENV: &str = "VMLAYER_CREDENTIAL_TOKEN";
/// Label listing the credentials a sandbox was granted.
pub const LABEL: &str = "vmlayer.credentials";
/// The name every sandbox resolves to the host.
pub const HOST_NAME: &str = "vmlayer.host";

/// One credential the proxy can inject.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// What sandboxes ask for, and the first path segment on the proxy.
    pub name: String,
    /// Base URL requests are forwarded to, e.g. `https://api.openai.com`.
    pub upstream: String,
    /// Header that carries the secret upstream, e.g. `authorization`.
    pub header: String,
    /// Header value, with `{secret}` replaced, e.g. `Bearer {secret}`.
    #[serde(default = "default_value")]
    pub value: String,
    /// Read the secret from this environment variable of the daemon...
    #[serde(default)]
    pub secret_env: Option<String>,
    /// ...or from this file (trailing whitespace trimmed).
    #[serde(default)]
    pub secret_file: Option<std::path::PathBuf>,
    /// Environment the sandbox gets; `{url}` is this credential's base URL
    /// on the proxy and `{placeholder}` the sandbox's placeholder, e.g.
    /// `{ OPENAI_BASE_URL = "{url}/v1", OPENAI_API_KEY = "{placeholder}" }`.
    #[serde(default)]
    pub guest_env: BTreeMap<String, String>,
    /// Only these tenants may use it (decision 0020). Empty: everyone.
    #[serde(default)]
    pub tenants: Vec<String>,
}

fn default_value() -> String {
    "{secret}".into()
}

/// `[credential_proxy]` in the daemon config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CredentialProxyConfig {
    /// Address to serve on. Guests reach it at `vmlayer.host:<port>`, so it
    /// must listen where sandbox traffic arrives (e.g. `0.0.0.0:7072`).
    pub listen: Option<String>,
    pub credentials: Vec<Credential>,
}

impl CredentialProxyConfig {
    pub fn port(&self) -> Option<u16> {
        self.listen
            .as_deref()
            .and_then(|l| l.parse::<SocketAddr>().ok())
            .map(|a| a.port())
    }

    pub fn get(&self, name: &str) -> Option<&Credential> {
        self.credentials.iter().find(|c| c.name == name)
    }

    /// Check the config at startup: unique names, readable secrets, URLs.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.credentials.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            self.port().is_some(),
            "[credential_proxy] listen must be host:port when credentials are configured"
        );
        let mut seen = std::collections::HashSet::new();
        for c in &self.credentials {
            anyhow::ensure!(
                seen.insert(&c.name) && !c.name.is_empty() && !c.name.contains('/'),
                "credential {:?}: names must be unique, non-empty and without '/'",
                c.name
            );
            anyhow::ensure!(
                c.upstream.starts_with("https://") || c.upstream.starts_with("http://"),
                "credential {:?}: upstream must be an http(s) URL",
                c.name
            );
            HeaderName::from_bytes(c.header.as_bytes())
                .with_context(|| format!("credential {:?}: bad header", c.name))?;
            secret(c).with_context(|| format!("credential {:?}", c.name))?;
        }
        Ok(())
    }
}

fn secret(c: &Credential) -> anyhow::Result<String> {
    match (&c.secret_env, &c.secret_file) {
        (Some(var), None) => std::env::var(var).with_context(|| format!("${var} is not set")),
        (None, Some(path)) => Ok(std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?
            .trim_end()
            .to_string()),
        _ => anyhow::bail!("set exactly one of secret_env and secret_file"),
    }
}

/// A fresh placeholder.
pub fn new_placeholder() -> String {
    format!("vmlayer-{}", uuid::Uuid::new_v4().simple())
}

pub fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The environment for a sandbox granted `names` with `placeholder`.
pub fn guest_env(
    cfg: &CredentialProxyConfig,
    names: &[String],
    placeholder: &str,
) -> BTreeMap<String, String> {
    let port = cfg.port().unwrap_or_default();
    let mut env = BTreeMap::from([(TOKEN_ENV.to_string(), placeholder.to_string())]);
    for c in names.iter().filter_map(|n| cfg.get(n)) {
        let url = format!("http://{HOST_NAME}:{port}/{}", c.name);
        for (k, v) in &c.guest_env {
            env.insert(
                k.clone(),
                v.replace("{url}", &url)
                    .replace("{placeholder}", placeholder),
            );
        }
    }
    env
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// The header holding a granted placeholder, and the placeholder.
fn find_placeholder(
    headers: &HeaderMap,
    granted: impl Fn(&[u8; 32]) -> bool,
) -> Option<(HeaderName, String)> {
    for (name, value) in headers {
        let Ok(v) = value.to_str() else { continue };
        for candidate in [v, v.strip_prefix("Bearer ").unwrap_or(v)] {
            if granted(&hash(candidate)) {
                return Some((name.clone(), candidate.to_string()));
            }
        }
    }
    None
}

type Body = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(
        Full::new(Bytes::from(format!("{msg}\n")))
            .map_err(|never| match never {})
            .boxed(),
    );
    *r.status_mut() = status;
    r
}

struct Proxy {
    daemon: Arc<Daemon>,
    client: reqwest::Client,
}

impl Proxy {
    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        let path = req.uri().path().to_string();
        let (name, rest) = path
            .trim_start_matches('/')
            .split_once('/')
            .map(|(n, r)| (n.to_string(), format!("/{r}")))
            .unwrap_or_else(|| (path.trim_start_matches('/').to_string(), "/".into()));
        let cfg = &self.daemon.cfg.credential_proxy;
        let Some(cred) = cfg.get(&name) else {
            return text(StatusCode::NOT_FOUND, "no such credential");
        };
        let store = &self.daemon.store;
        let granted = |h: &[u8; 32]| {
            store
                .credential_grant(h)
                .ok()
                .flatten()
                .is_some_and(|names| names.iter().any(|n| n == &name))
        };
        let Some((carrier, _placeholder)) = find_placeholder(req.headers(), granted) else {
            return text(
                StatusCode::UNAUTHORIZED,
                "no placeholder granted this credential",
            );
        };
        let secret = match secret(cred) {
            Ok(s) => s,
            Err(e) => return text(StatusCode::BAD_GATEWAY, &format!("{e:#}")),
        };
        let mut url = format!("{}{rest}", cred.upstream.trim_end_matches('/'));
        if let Some(q) = req.uri().query() {
            url.push('?');
            url.push_str(q);
        }
        let mut headers = req.headers().clone();
        for h in HOP_BY_HOP {
            headers.remove(*h);
        }
        headers.remove(&carrier);
        let header = HeaderName::from_bytes(cred.header.as_bytes()).expect("validated");
        match HeaderValue::from_str(&cred.value.replace("{secret}", &secret)) {
            Ok(v) => {
                headers.insert(header, v);
            }
            Err(_) => return text(StatusCode::BAD_GATEWAY, "secret is not a valid header"),
        }
        let method = req.method().clone();
        let body = reqwest::Body::wrap_stream(
            BodyStream::new(req.into_body())
                .try_filter_map(|f| async move { Ok(f.into_data().ok()) }),
        );
        let upstream = match self
            .client
            .request(method, url)
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return text(StatusCode::BAD_GATEWAY, &format!("upstream: {e}")),
        };
        let mut resp = Response::builder().status(upstream.status());
        for (k, v) in upstream.headers() {
            if !HOP_BY_HOP.contains(&k.as_str()) {
                resp = resp.header(k, v);
            }
        }
        let stream = upstream
            .bytes_stream()
            .map_ok(Frame::data)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
        resp.body(BodyExt::boxed(StreamBody::new(stream)))
            .unwrap_or_else(|e| text(StatusCode::BAD_GATEWAY, &e.to_string()))
    }
}

/// Serve the proxy until the process exits. Off unless `listen` is set and
/// credentials are configured.
pub async fn serve(daemon: Arc<Daemon>) -> anyhow::Result<()> {
    let cfg = daemon.cfg.credential_proxy.clone();
    let Some(listen) = cfg.listen.clone().filter(|_| !cfg.credentials.is_empty()) else {
        return Ok(());
    };
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding the credential proxy to {listen}"))?;
    tracing::info!(%listen, credentials = cfg.credentials.len(), "credential proxy listening");
    let proxy = Arc::new(Proxy {
        daemon,
        client: reqwest::Client::builder()
            .build()
            .context("building the upstream client")?,
    });
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let proxy = proxy.clone();
                async move { Ok::<_, std::convert::Infallible>(proxy.handle(req).await) }
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// The credentials `sandbox` holds again after a fork or restore: its
/// labels name them and its environment carries the placeholder.
pub fn regrant(
    daemon: &Daemon,
    sandbox: &SandboxId,
    labels: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) {
    let (Some(names), Some(token)) = (labels.get(LABEL), env.get(TOKEN_ENV)) else {
        return;
    };
    let names: Vec<String> = names.split(',').map(String::from).collect();
    if let Err(e) = daemon
        .store
        .put_credential_grant(&hash(token), sandbox, &names)
    {
        tracing::warn!(%sandbox, error = %e, "could not restore the sandbox's credential grant");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CredentialProxyConfig {
        toml::from_str(
            r#"
listen = "0.0.0.0:7072"
[[credentials]]
name = "openai"
upstream = "https://api.openai.com"
header = "authorization"
value = "Bearer {secret}"
secret_env = "VMLAYER_TEST_OPENAI_KEY"
guest_env = { OPENAI_BASE_URL = "{url}/v1", OPENAI_API_KEY = "{placeholder}" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn guests_get_urls_and_placeholders_only() {
        let env = guest_env(&cfg(), &["openai".into()], "vmlayer-p");
        assert_eq!(env["OPENAI_BASE_URL"], "http://vmlayer.host:7072/openai/v1");
        assert_eq!(env["OPENAI_API_KEY"], "vmlayer-p");
        assert_eq!(env[TOKEN_ENV], "vmlayer-p");
    }

    #[test]
    fn config_is_checked() {
        std::env::set_var("VMLAYER_TEST_OPENAI_KEY", "sk-real");
        cfg().validate().unwrap();
        let mut dup = cfg();
        dup.credentials.push(dup.credentials[0].clone());
        assert!(dup.validate().is_err());
        let mut no_secret = cfg();
        no_secret.credentials[0].secret_env = Some("VMLAYER_TEST_UNSET_VAR".into());
        assert!(no_secret.validate().is_err());
    }

    #[test]
    fn the_placeholder_is_found_in_any_header() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("other"));
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer vmlayer-p"),
        );
        let want = hash("vmlayer-p");
        let found = find_placeholder(&h, |x| *x == want).unwrap();
        assert_eq!(
            found,
            (HeaderName::from_static("authorization"), "vmlayer-p".into())
        );
        assert!(find_placeholder(&h, |_| false).is_none());
    }
}
