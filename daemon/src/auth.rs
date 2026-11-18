//! Who is calling and what they may do (decision 0020).
//!
//! A TCP caller presents a bearer token. The daemon maps the token to a
//! [`Principal`]: a name for logs, an optional tenant and a role. The role
//! decides which RPCs the caller may call; the tenant decides which
//! sandboxes, volumes and snapshots it can see. A caller on the unix socket
//! is the local administrator: file permissions on the socket are its
//! authentication.
//!
//! Tenancy is a reserved label, [`TENANT_LABEL`], on every sandbox, volume,
//! volume snapshot and sandbox snapshot. A tenant's requests stamp it on
//! what they create and only see resources that carry it. Resources made
//! from others (a fork, a restore, a clone, a snapshot) inherit it. Volume
//! names are unique per tenant: the daemon stores `tenant/name`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tonic::Status;

/// Label that holds the owning tenant of a resource.
pub const TENANT_LABEL: &str = "vmlayer.tenant";

/// What a caller may do. Each role includes the ones before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Get, list, read files, read process output.
    Read,
    /// Everything that changes sandboxes, volumes and snapshots.
    #[default]
    Write,
    /// Host-wide operations: blob store garbage collection.
    Admin,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        })
    }
}

/// One accepted token, as written in the config file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    /// Shown in logs; never the secret.
    pub name: String,
    /// Hex SHA-256 of the token, e.g. from `printf %s "$TOKEN" | sha256sum`.
    /// The daemon never needs the token itself.
    pub sha256: String,
    /// Limit the token to one tenant's resources. Unset sees everything.
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub role: Role,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub tokens: Vec<TokenConfig>,
}

/// TLS for TCP listeners. With `client_ca`, callers must also present a
/// certificate signed by it (mutual TLS).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// PEM certificate chain of the daemon.
    pub cert: Option<PathBuf>,
    /// PEM private key of the daemon.
    pub key: Option<PathBuf>,
    /// PEM CA bundle that client certificates must chain to.
    pub client_ca: Option<PathBuf>,
}

impl TlsConfig {
    pub fn enabled(&self) -> bool {
        self.cert.is_some() || self.key.is_some()
    }
}

/// The caller of one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub tenant: Option<String>,
    pub role: Role,
}

impl Principal {
    /// A caller on the unix socket.
    pub fn local() -> Self {
        Self {
            name: "local".into(),
            tenant: None,
            role: Role::Admin,
        }
    }

    /// The caller of `req`. Requests that did not pass the token check
    /// came in on the unix socket.
    pub fn of<T>(req: &tonic::Request<T>) -> Self {
        req.extensions()
            .get::<Principal>()
            .cloned()
            .unwrap_or_else(Self::local)
    }

    /// Mark labels of a resource this caller creates, or narrow a list
    /// filter to this caller's tenant.
    pub fn stamp(&self, labels: &mut BTreeMap<String, String>) {
        if let Some(t) = &self.tenant {
            labels.insert(TENANT_LABEL.into(), t.clone());
        }
    }

    /// Whether this caller may see a resource with these labels.
    pub fn sees(&self, labels: &BTreeMap<String, String>) -> bool {
        match &self.tenant {
            None => true,
            Some(t) => labels.get(TENANT_LABEL) == Some(t),
        }
    }
}

/// Only the tenant label of `labels`, for resources made on its behalf.
pub fn tenant_only(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    labels
        .get(TENANT_LABEL)
        .map(|t| BTreeMap::from([(TENANT_LABEL.to_string(), t.clone())]))
        .unwrap_or_default()
}

/// Give a derived resource the tenant of its source, unless one is set.
pub fn inherit_tenant(labels: &mut BTreeMap<String, String>, source: &BTreeMap<String, String>) {
    if let Some(t) = source.get(TENANT_LABEL) {
        labels
            .entry(TENANT_LABEL.to_string())
            .or_insert_with(|| t.clone());
    }
}

/// The stored name of a volume named `name` with these labels.
pub fn scoped_volume_name(name: String, labels: &BTreeMap<String, String>) -> String {
    match labels.get(TENANT_LABEL) {
        Some(t) if !name.starts_with(&format!("{t}/")) => format!("{t}/{name}"),
        _ => name,
    }
}

impl Principal {
    /// The name this caller sees for a stored volume name: a tenant sees
    /// `ws` for its `tenant/ws`; the operator sees the stored name, which
    /// is also what it passes to address the volume.
    pub fn volume_name<'a>(&self, stored: &'a str) -> &'a str {
        self.tenant
            .as_deref()
            .and_then(|t| stored.strip_prefix(t))
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(stored)
    }
}

/// The role an RPC needs, by its gRPC path (`/vmlayer.v1.Service/Method`).
/// Methods not listed need [`Role::Write`], so a new RPC is never readable
/// by accident.
pub fn required_role(path: &str) -> Role {
    let method = path.strip_prefix("/vmlayer.v1.").unwrap_or(path);
    match method {
        "Sandboxes/Get"
        | "Sandboxes/List"
        | "Sandboxes/WaitForPort"
        | "Processes/Output"
        | "Processes/Wait"
        | "Processes/List"
        | "Files/Read"
        | "Files/Stat"
        | "Files/List"
        | "Volumes/Get"
        | "Volumes/List"
        | "Volumes/ListSnapshots"
        | "System/Info"
        | "System/Health"
        | "System/ListSnapshots"
        | "System/ListHosts"
        | "System/PlaceSandbox" => Role::Read,
        "System/CollectGarbage" => Role::Admin,
        _ => Role::Write,
    }
}

fn valid_tenant(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 63
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// The accepted tokens, by the SHA-256 of their value.
#[derive(Debug, Default)]
pub struct Tokens {
    by_hash: HashMap<[u8; 32], Principal>,
}

impl Tokens {
    /// `legacy` is the single `--token`: an administrator of every tenant.
    pub fn new(legacy: Option<&str>, cfg: &AuthConfig) -> anyhow::Result<Self> {
        let mut by_hash = HashMap::new();
        if let Some(t) = legacy.filter(|t| !t.is_empty()) {
            by_hash.insert(
                Sha256::digest(t.as_bytes()).into(),
                Principal {
                    name: "token".into(),
                    tenant: None,
                    role: Role::Admin,
                },
            );
        }
        for t in &cfg.tokens {
            let hash = decode_sha256(&t.sha256).ok_or_else(|| {
                anyhow::anyhow!("token {:?}: sha256 must be 64 hex characters", t.name)
            })?;
            if let Some(tenant) = &t.tenant {
                anyhow::ensure!(
                    valid_tenant(tenant),
                    "token {:?}: tenant {tenant:?} must be 1-63 characters of [A-Za-z0-9._-]",
                    t.name
                );
            }
            let p = Principal {
                name: t.name.clone(),
                tenant: t.tenant.clone(),
                role: t.role,
            };
            anyhow::ensure!(
                by_hash.insert(hash, p).is_none(),
                "token {:?} has the same value as another token",
                t.name
            );
        }
        Ok(Self { by_hash })
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// The principal of a presented token. Hashing first means the lookup
    /// time does not depend on how much of a guessed token is right.
    pub fn lookup(&self, token: &str) -> Option<&Principal> {
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.by_hash.get(&hash)
    }
}

fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Middleware for TCP listeners: authenticate the token, check the role,
/// and hand the principal to the service in the request extensions.
pub async fn authenticate(
    axum::extract::State(tokens): axum::extract::State<Arc<Tokens>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let principal = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| tokens.lookup(t))
        .cloned();
    let Some(p) = principal else {
        return reject(Status::unauthenticated("missing or invalid bearer token"));
    };
    let need = required_role(req.uri().path());
    if p.role < need {
        return reject(Status::permission_denied(format!(
            "{} needs the {need} role; token {:?} has {}",
            req.uri().path(),
            p.name,
            p.role
        )));
    }
    req.extensions_mut().insert(p);
    next.run(req).await
}

fn reject(s: Status) -> axum::response::Response {
    s.into_http::<axum::body::Body>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> String {
        Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn tokens_map_to_principals_by_hash() {
        let cfg: AuthConfig = toml::from_str(&format!(
            "[[tokens]]\nname = \"ci\"\nsha256 = \"{}\"\ntenant = \"acme\"\nrole = \"read\"\n",
            hex("s3cret")
        ))
        .unwrap();
        let tokens = Tokens::new(Some("legacy"), &cfg).unwrap();
        let p = tokens.lookup("s3cret").unwrap();
        assert_eq!(p.tenant.as_deref(), Some("acme"));
        assert_eq!(p.role, Role::Read);
        assert_eq!(tokens.lookup("legacy").unwrap().role, Role::Admin);
        assert!(tokens.lookup("s3cre").is_none());
    }

    #[test]
    fn bad_token_config_is_refused() {
        let bad_hash = AuthConfig {
            tokens: vec![TokenConfig {
                name: "x".into(),
                sha256: "abc".into(),
                tenant: None,
                role: Role::Write,
            }],
        };
        assert!(Tokens::new(None, &bad_hash).is_err());
        let bad_tenant = AuthConfig {
            tokens: vec![TokenConfig {
                name: "x".into(),
                sha256: hex("a"),
                tenant: Some("a/b".into()),
                role: Role::Write,
            }],
        };
        assert!(Tokens::new(None, &bad_tenant).is_err());
    }

    #[test]
    fn roles_by_method() {
        assert_eq!(required_role("/vmlayer.v1.Sandboxes/List"), Role::Read);
        assert_eq!(required_role("/vmlayer.v1.Sandboxes/Create"), Role::Write);
        assert_eq!(required_role("/vmlayer.v1.Processes/Exec"), Role::Write);
        assert_eq!(
            required_role("/vmlayer.v1.System/CollectGarbage"),
            Role::Admin
        );
        assert_eq!(
            required_role("/vmlayer.v1.Sandboxes/SomethingNew"),
            Role::Write
        );
        assert!(Role::Admin > Role::Write && Role::Write > Role::Read);
    }

    #[test]
    fn tenants_see_only_their_resources() {
        let acme = Principal {
            name: "a".into(),
            tenant: Some("acme".into()),
            role: Role::Write,
        };
        let mut labels = BTreeMap::from([(TENANT_LABEL.to_string(), "other".to_string())]);
        acme.stamp(&mut labels);
        assert_eq!(labels[TENANT_LABEL], "acme");
        assert!(acme.sees(&labels));
        assert!(!acme.sees(&BTreeMap::new()));
        assert!(Principal::local().sees(&BTreeMap::new()));

        let name = scoped_volume_name("ws".into(), &labels);
        assert_eq!(name, "acme/ws");
        assert_eq!(scoped_volume_name(name.clone(), &labels), "acme/ws");
        assert_eq!(acme.volume_name(&name), "ws");
        assert_eq!(Principal::local().volume_name(&name), "acme/ws");
        assert_eq!(acme.volume_name("plain"), "plain");

        let mut derived = BTreeMap::new();
        inherit_tenant(&mut derived, &labels);
        assert_eq!(derived, tenant_only(&labels));
    }
}
