//! Pulling an image: manifest resolution for the host platform, layers in
//! memory, and the parts of the OCI config a sandbox needs.

use std::collections::BTreeMap;

use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::{Deserialize, Serialize};
use vmlayer_core::Result;

use super::err;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

impl ImageConfig {
    pub fn entrypoint_argv(&self) -> Option<Vec<String>> {
        let mut v = self.entrypoint.clone();
        v.extend(self.cmd.iter().cloned());
        (!v.is_empty()).then_some(v)
    }

    pub fn env_map(&self) -> BTreeMap<String, String> {
        self.env
            .iter()
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    }

    /// From the OCI image config JSON (`{"config": {...}}`).
    pub fn from_oci_json(json: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Outer {
            #[serde(default)]
            config: Inner,
        }
        #[derive(Deserialize, Default)]
        struct Inner {
            #[serde(rename = "Entrypoint", default)]
            entrypoint: Option<Vec<String>>,
            #[serde(rename = "Cmd", default)]
            cmd: Option<Vec<String>>,
            #[serde(rename = "Env", default)]
            env: Option<Vec<String>>,
            #[serde(rename = "WorkingDir", default)]
            working_dir: Option<String>,
            #[serde(rename = "User", default)]
            user: Option<String>,
        }
        let outer: Outer = serde_json::from_slice(json).unwrap_or(Outer {
            config: Inner::default(),
        });
        Self {
            entrypoint: outer.config.entrypoint.unwrap_or_default(),
            cmd: outer.config.cmd.unwrap_or_default(),
            env: outer.config.env.unwrap_or_default(),
            working_dir: outer.config.working_dir,
            user: outer.config.user,
        }
    }
}

pub struct Layer {
    pub data: bytes::Bytes,
    pub media_type: String,
}

pub struct Pulled {
    pub digest: String,
    pub config: ImageConfig,
    pub layers: Vec<Layer>,
}

/// Registry credentials from `~/.docker/config.json`, if any for this registry.
fn docker_config_auth(registry: &str) -> RegistryAuth {
    let Some(home) = std::env::var_os("HOME") else {
        return RegistryAuth::Anonymous;
    };
    let path = std::path::PathBuf::from(home)
        .join(".docker")
        .join("config.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return RegistryAuth::Anonymous;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return RegistryAuth::Anonymous;
    };
    let auths = json.get("auths").and_then(|a| a.as_object());
    let keys = [
        registry.to_string(),
        format!("https://{registry}"),
        format!("https://{registry}/v1/"),
    ];
    let entry = auths.and_then(|a| keys.iter().find_map(|k| a.get(k)));
    let Some(b64) = entry.and_then(|e| e.get("auth")).and_then(|v| v.as_str()) else {
        return RegistryAuth::Anonymous;
    };
    use base64_decode as decode;
    match decode(b64).and_then(|s| {
        s.split_once(':')
            .map(|(u, p)| (u.to_string(), p.to_string()))
    }) {
        Some((u, p)) => RegistryAuth::Basic(u, p),
        None => RegistryAuth::Anonymous,
    }
}

/// Minimal standard base64 decoder (no padding tricks needed for docker auth).
fn base64_decode(s: &str) -> Option<String> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = T.iter().position(|&t| t == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// The OCI architecture name for the machine the daemon runs on.
pub fn host_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// Pick the `linux/<host arch>` manifest from an image index. Guests are
/// always Linux, whatever the daemon's own OS is (the library's default
/// resolver matches the host OS, which is wrong on a macOS dev machine).
fn linux_host_arch_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    let arch = host_oci_arch();
    manifests
        .iter()
        .find(|entry| {
            entry
                .platform
                .as_ref()
                .is_some_and(|p| p.os.to_string() == "linux" && p.architecture.to_string() == arch)
        })
        .map(|entry| entry.digest.clone())
}

pub async fn pull(reference: &str) -> Result<Pulled> {
    let reference: Reference = reference
        .parse()
        .map_err(|e| err(format!("bad image reference {reference:?}: {e}")))?;
    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        platform_resolver: Some(Box::new(linux_host_arch_resolver)),
        ..Default::default()
    };
    let client = Client::new(config);
    let auth = docker_config_auth(reference.resolve_registry());
    let accepted = vec![
        oci_client::manifest::IMAGE_LAYER_MEDIA_TYPE,
        oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
        oci_client::manifest::IMAGE_LAYER_NONDISTRIBUTABLE_MEDIA_TYPE,
        oci_client::manifest::IMAGE_LAYER_NONDISTRIBUTABLE_GZIP_MEDIA_TYPE,
        oci_client::manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
        oci_client::manifest::IMAGE_DOCKER_LAYER_TAR_MEDIA_TYPE,
        "application/vnd.oci.image.layer.v1.tar+zstd",
    ];
    tracing::info!(image = %reference, "pulling image");
    let data = client
        .pull(&reference, &auth, accepted)
        .await
        .map_err(|e| err(format!("pulling {reference}: {e}")))?;
    let digest = data
        .digest
        .clone()
        .unwrap_or_else(|| format!("sha256:{}", hex_sha256(&data.config.data)));
    Ok(Pulled {
        digest,
        config: ImageConfig::from_oci_json(&data.config.data),
        layers: data
            .layers
            .into_iter()
            .map(|l| Layer {
                data: l.data,
                media_type: l.media_type,
            })
            .collect(),
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_and_env() {
        let json = br#"{"architecture":"arm64","config":{"Env":["PATH=/usr/bin","LANG=C.UTF-8"],"Entrypoint":["python3"],"Cmd":["-i"],"WorkingDir":"/app"}}"#;
        let c = ImageConfig::from_oci_json(json);
        assert_eq!(
            c.entrypoint_argv(),
            Some(vec!["python3".into(), "-i".into()])
        );
        assert_eq!(c.env_map()["LANG"], "C.UTF-8");
        assert_eq!(c.working_dir.as_deref(), Some("/app"));
        let empty = ImageConfig::from_oci_json(b"{}");
        assert_eq!(empty.entrypoint_argv(), None);
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(base64_decode("dXNlcjpwYXNz").as_deref(), Some("user:pass"));
        assert_eq!(base64_decode("YQ==").as_deref(), Some("a"));
    }
}
