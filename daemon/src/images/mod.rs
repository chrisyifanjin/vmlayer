//! The image pipeline for microVM backends: OCI reference → cached ext4
//! root image plus the image config. See ADR 0009.

mod ext4;
mod oci;
mod unpack;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use vmlayer_core::{Error, ImageResolver, ResolvedImage, Result, Rootfs};

pub use oci::ImageConfig;

fn err(message: impl Into<String>) -> Error {
    Error::Engine {
        backend: "images",
        message: message.into(),
        source: None,
    }
}

/// What the cache knows about one built image.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuiltImage {
    pub reference: String,
    pub digest: String,
    pub rootfs: PathBuf,
    pub config: ImageConfig,
    pub size_bytes: u64,
}

pub struct ImageStore {
    root: PathBuf,
    /// Serialises builds so two sandboxes asking for the same new image do
    /// not both build it.
    build_lock: Mutex<()>,
}

impl ImageStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("images"),
            build_lock: Mutex::new(()),
        }
    }

    fn digest_dir(&self, digest: &str) -> PathBuf {
        self.root.join(digest.replace(':', "-"))
    }

    fn by_ref_file(&self, reference: &str) -> PathBuf {
        let safe: String = reference
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join("by-ref").join(safe)
    }

    /// Cached result for a reference, if any.
    async fn cached(&self, reference: &str) -> Option<BuiltImage> {
        let digest = tokio::fs::read_to_string(self.by_ref_file(reference))
            .await
            .ok()?;
        let meta = tokio::fs::read(self.digest_dir(digest.trim()).join("meta.json"))
            .await
            .ok()?;
        let built: BuiltImage = serde_json::from_slice(&meta).ok()?;
        built.rootfs.exists().then_some(built)
    }

    /// Pull, unpack and build if not cached. `pull_latest` re-resolves a
    /// tag even when a build for it exists.
    pub async fn ensure(&self, reference: &str, pull_latest: bool) -> Result<BuiltImage> {
        if !pull_latest {
            if let Some(b) = self.cached(reference).await {
                return Ok(b);
            }
        }
        let _guard = self.build_lock.lock().await;
        if !pull_latest {
            if let Some(b) = self.cached(reference).await {
                return Ok(b);
            }
        }
        ext4::check_tools()?;
        tokio::fs::create_dir_all(self.root.join("by-ref")).await?;
        let pulled = oci::pull(reference).await?;
        let dir = self.digest_dir(&pulled.digest);
        let rootfs = dir.join("rootfs.ext4");
        if !rootfs.exists() {
            let scratch = self
                .root
                .join(format!(".build-{}", uuid::Uuid::now_v7().simple()));
            let result: Result<u64> = async {
                tokio::fs::create_dir_all(&scratch).await?;
                let unpacked = unpack::apply_layers(&scratch, &pulled.layers).await?;
                tokio::fs::create_dir_all(&dir).await?;
                let tmp = dir.join("rootfs.ext4.partial");
                ext4::build(&scratch, &tmp, unpacked).await?;
                tokio::fs::rename(&tmp, &rootfs).await?;
                Ok(unpacked)
            }
            .await;
            let _ = tokio::fs::remove_dir_all(&scratch).await;
            if let Err(e) = result {
                let _ = tokio::fs::remove_file(dir.join("rootfs.ext4.partial")).await;
                return Err(e);
            }
        }
        let size_bytes = tokio::fs::metadata(&rootfs)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let built = BuiltImage {
            reference: reference.to_string(),
            digest: pulled.digest.clone(),
            rootfs: rootfs.clone(),
            config: pulled.config.clone(),
            size_bytes,
        };
        tokio::fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&built).map_err(|e| err(e.to_string()))?,
        )
        .await?;
        tokio::fs::write(self.by_ref_file(reference), &pulled.digest).await?;
        tracing::info!(reference, digest = %pulled.digest, size_bytes, "image ready");
        Ok(built)
    }

    pub async fn list(&self) -> Result<Vec<BuiltImage>> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(&self.root).await else {
            return Ok(out);
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Ok(bytes) = tokio::fs::read(entry.path().join("meta.json")).await {
                if let Ok(b) = serde_json::from_slice::<BuiltImage>(&bytes) {
                    out.push(b);
                }
            }
        }
        Ok(out)
    }
}

/// `ImageResolver` for microVM backends: a path to an ext4 file is used as
/// is; anything else is an OCI reference built through the store.
pub struct MicrovmImageResolver {
    store: std::sync::Arc<ImageStore>,
}

impl MicrovmImageResolver {
    pub fn new(store: std::sync::Arc<ImageStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ImageResolver for MicrovmImageResolver {
    async fn resolve(&self, image: &str) -> Result<ResolvedImage> {
        let path = Path::new(image);
        if path.is_absolute() {
            if !path.exists() {
                return Err(Error::Guest(format!("root image not found: {image}")));
            }
            return Ok(ResolvedImage {
                rootfs: Rootfs::Ext4 {
                    path: path.to_path_buf(),
                },
                entrypoint: None,
                workdir: None,
                env: BTreeMap::new(),
            });
        }
        let built = self.store.ensure(image, false).await?;
        Ok(ResolvedImage {
            rootfs: Rootfs::Ext4 { path: built.rootfs },
            entrypoint: built.config.entrypoint_argv(),
            workdir: built.config.working_dir.clone().filter(|w| !w.is_empty()),
            env: built.config.env_map(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pulls a real image and unpacks it (no mkfs needed). Network: gated.
    #[tokio::test]
    async fn pulls_and_unpacks_alpine() {
        if std::env::var("VMLAYER_NET_TESTS").as_deref() != Ok("1") {
            eprintln!("VMLAYER_NET_TESTS != 1; skipping");
            return;
        }
        let pulled = oci::pull("alpine:3.20").await.unwrap();
        assert!(pulled.digest.starts_with("sha256:"));
        assert_eq!(
            pulled.config.entrypoint_argv(),
            Some(vec!["/bin/sh".to_string()])
        );
        let dir = tempfile::tempdir().unwrap();
        let n = unpack::apply_layers(dir.path(), &pulled.layers)
            .await
            .unwrap();
        assert!(n > 1_000_000, "unpacked {n} bytes");
        assert!(dir.path().join("bin/busybox").exists());
        assert!(dir.path().join("etc/alpine-release").exists());
        let meta = std::fs::symlink_metadata(dir.path().join("bin/sh")).unwrap();
        assert!(meta.file_type().is_symlink() || meta.is_file());
    }

    /// The whole pipeline: pull, unpack, mkfs.ext4 -d, cache. Needs network
    /// and e2fsprogs, so it runs on Linux CI and in the dev container.
    #[tokio::test]
    async fn ensure_builds_an_ext4_and_caches_it() {
        if std::env::var("VMLAYER_NET_TESTS").as_deref() != Ok("1") || ext4::check_tools().is_err()
        {
            eprintln!("needs VMLAYER_NET_TESTS=1 and mkfs.ext4; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(ImageStore::new(dir.path()));
        let built = store.ensure("alpine:3.20", false).await.unwrap();
        assert!(built.rootfs.ends_with("rootfs.ext4") && built.rootfs.exists());
        assert!(built.size_bytes >= 256 * 1024 * 1024);
        // ext4 superblock magic.
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&built.rootfs).unwrap();
        f.seek(SeekFrom::Start(0x438)).unwrap();
        let mut magic = [0u8; 2];
        f.read_exact(&mut magic).unwrap();
        assert_eq!(magic, [0x53, 0xef]);
        // The files are really inside: debugfs reads them without mounting.
        let out = std::process::Command::new("debugfs")
            .args(["-R", "cat /etc/alpine-release"])
            .arg(&built.rootfs)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).starts_with("3.20"),
            "{out:?}"
        );
        // Second call hits the cache (same file, no rebuild).
        let mtime = std::fs::metadata(&built.rootfs)
            .unwrap()
            .modified()
            .unwrap();
        let again = store.ensure("alpine:3.20", false).await.unwrap();
        assert_eq!(again.digest, built.digest);
        assert_eq!(
            std::fs::metadata(&again.rootfs)
                .unwrap()
                .modified()
                .unwrap(),
            mtime
        );
        assert_eq!(store.list().await.unwrap().len(), 1);
        // And the resolver hands a backend what it needs.
        let r = MicrovmImageResolver::new(store)
            .resolve("alpine:3.20")
            .await
            .unwrap();
        assert!(matches!(r.rootfs, Rootfs::Ext4 { .. }));
        assert_eq!(r.entrypoint, Some(vec!["/bin/sh".to_string()]));
        assert!(r.env.contains_key("PATH"));
    }
}
