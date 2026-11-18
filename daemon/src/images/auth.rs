//! Registry credentials, resolved the way `docker pull` does (review T3.2).
//!
//! The config is `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`.
//! For a registry, in order:
//!
//! 1. `credHelpers[registry]`: run `docker-credential-<name> get`;
//! 2. `auths[registry].auth`: inline `user:password` (base64);
//! 3. `credsStore`: run that helper.
//!
//! Helpers are how the cloud registries hand out short-lived credentials:
//! `docker-credential-ecr-login` (Amazon ECR), `docker-credential-gcloud` or
//! `docker-credential-gcr` (Google Artifact Registry and GCR),
//! `docker-credential-acr-env` (Azure Container Registry), and the desktop
//! keychains. Any helper that speaks the protocol works.

use std::path::PathBuf;
use std::time::Duration;

use oci_client::secrets::RegistryAuth;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

/// What the config says to use for one registry.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    Helper(String),
    Basic(String, String),
    Anonymous,
}

fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker").join("config.json"))
}

/// The names a registry may be stored under in `config.json`.
fn keys(registry: &str) -> Vec<String> {
    let mut k = vec![
        registry.to_string(),
        format!("https://{registry}"),
        format!("https://{registry}/v1/"),
        format!("http://{registry}"),
    ];
    if matches!(
        registry,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    ) {
        k.push("https://index.docker.io/v1/".into());
        k.push("index.docker.io".into());
    }
    k
}

fn lookup(config: &serde_json::Value, registry: &str) -> Source {
    let keys = keys(registry);
    let find = |section: &str| {
        config
            .get(section)
            .and_then(|s| s.as_object())
            .and_then(|m| keys.iter().find_map(|k| m.get(k)))
    };
    if let Some(helper) = find("credHelpers").and_then(|v| v.as_str()) {
        return Source::Helper(helper.to_string());
    }
    if let Some(user_pass) = find("auths")
        .and_then(|e| e.get("auth"))
        .and_then(|v| v.as_str())
        .and_then(super::oci::base64_decode)
        .and_then(|s| {
            s.split_once(':')
                .map(|(u, p)| (u.to_string(), p.to_string()))
        })
    {
        return Source::Basic(user_pass.0, user_pass.1);
    }
    if let Some(store) = config.get("credsStore").and_then(|v| v.as_str()) {
        return Source::Helper(store.to_string());
    }
    Source::Anonymous
}

#[derive(Deserialize)]
struct HelperReply {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

/// Username helpers use for an identity (refresh) token; registries such as
/// ACR take it with this fixed user name in basic auth.
const TOKEN_USER: &str = "<token>";
const TOKEN_BASIC_USER: &str = "00000000-0000-0000-0000-000000000000";

async fn run_helper(helper: &str, registry: &str) -> Result<RegistryAuth, String> {
    let program = format!("docker-credential-{helper}");
    let mut child = tokio::process::Command::new(&program)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(registry.as_bytes()).await;
    }
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .map_err(|_| format!("{program} timed out"))?
        .map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        // "credentials not found" is a normal answer: pull anonymously.
        let msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return if msg.contains("credentials not found") {
            Ok(RegistryAuth::Anonymous)
        } else {
            Err(format!(
                "{program} failed: {msg} {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        };
    }
    let reply: HelperReply =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("{program}: bad reply: {e}"))?;
    Ok(if reply.username == TOKEN_USER {
        RegistryAuth::Basic(TOKEN_BASIC_USER.into(), reply.secret)
    } else {
        RegistryAuth::Basic(reply.username, reply.secret)
    })
}

/// Credentials for `registry`, or anonymous access.
pub async fn registry_auth(registry: &str) -> RegistryAuth {
    let config = config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let Some(config) = config else {
        return RegistryAuth::Anonymous;
    };
    match lookup(&config, registry) {
        Source::Anonymous => RegistryAuth::Anonymous,
        Source::Basic(u, p) => RegistryAuth::Basic(u, p),
        Source::Helper(helper) => match run_helper(&helper, registry).await {
            Ok(auth) => auth,
            Err(e) => {
                tracing::warn!(%registry, error = %e, "credential helper failed; pulling anonymously");
                RegistryAuth::Anonymous
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_win_over_inline_auth_and_the_store_comes_last() {
        let cfg = serde_json::json!({
            "credHelpers": { "123.dkr.ecr.us-east-1.amazonaws.com": "ecr-login" },
            "auths": {
                "123.dkr.ecr.us-east-1.amazonaws.com": { "auth": "dTpw" },
                "ghcr.io": { "auth": "dTpw" },
                "https://index.docker.io/v1/": { "auth": "aHViOnNlY3JldA==" }
            },
            "credsStore": "desktop"
        });
        assert_eq!(
            lookup(&cfg, "123.dkr.ecr.us-east-1.amazonaws.com"),
            Source::Helper("ecr-login".into())
        );
        assert_eq!(
            lookup(&cfg, "ghcr.io"),
            Source::Basic("u".into(), "p".into())
        );
        assert_eq!(
            lookup(&cfg, "index.docker.io"),
            Source::Basic("hub".into(), "secret".into())
        );
        assert_eq!(
            lookup(&cfg, "myacr.azurecr.io"),
            Source::Helper("desktop".into())
        );
        assert_eq!(lookup(&serde_json::json!({}), "x.io"), Source::Anonymous);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helpers_speak_the_docker_protocol() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let p = dir.path().join(format!("docker-credential-{name}"));
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // Echoes the registry it was asked about into the user name.
        write(
            "fake",
            r#"read r; printf '{"ServerURL":"%s","Username":"u-%s","Secret":"s"}' "$r" "$r""#,
        );
        write(
            "token",
            r#"printf '{"Username":"<token>","Secret":"refresh"}'"#,
        );
        write(
            "none",
            r#"echo "credentials not found in native keychain"; exit 1"#,
        );
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
        let auth = run_helper("fake", "reg.example").await.unwrap();
        assert!(matches!(auth, RegistryAuth::Basic(u, s) if u == "u-reg.example" && s == "s"));
        let auth = run_helper("token", "x").await.unwrap();
        assert!(matches!(auth, RegistryAuth::Basic(u, _) if u == TOKEN_BASIC_USER));
        assert!(matches!(
            run_helper("none", "x").await.unwrap(),
            RegistryAuth::Anonymous
        ));
        assert!(run_helper("missing-helper", "x").await.is_err());
    }
}
