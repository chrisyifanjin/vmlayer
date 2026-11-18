//! On-disk boot templates for creates without mounted volumes.
//!
//! The template guest has no caller env, entrypoint, workspace or data. It is
//! booted once, snapshotted after the agent is ready, then killed. Restores
//! allocate a new VMM and guest memory on demand; no idle VM is kept alive.
//!
//! One template exists per image and VM shape, each the size of the guest's
//! memory plus its root disk. The most recently used `template_limit` are
//! kept; older ones, and ones for images that changed, are deleted.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use uuid::Uuid;
use vmlayer_core::{
    ExecRequest, Result, SandboxHandle, SandboxId, SandboxSpec, SnapshotOptions, SnapshotRef,
};

use crate::sandbox::cpu_model;
use crate::FirecrackerBackend;

fn file_stamp(path: &Path) -> (u64, u128) {
    std::fs::metadata(path)
        .ok()
        .map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (m.len(), modified)
        })
        .unwrap_or((0, 0))
}

impl FirecrackerBackend {
    pub(crate) fn template_ref_path(&self, spec: &SandboxSpec, base: &Path) -> PathBuf {
        let c = &self.config;
        let mut hasher = Sha256::new();
        for path in [base, &c.kernel, &c.firecracker_bin] {
            let (size, modified) = file_stamp(path);
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(size.to_le_bytes());
            hasher.update(modified.to_le_bytes());
        }
        if let Some(agent) = &c.guest_agent {
            let (size, modified) = file_stamp(agent);
            hasher.update(agent.to_string_lossy().as_bytes());
            hasher.update(size.to_le_bytes());
            hasher.update(modified.to_le_bytes());
        }
        // Templates for sandboxes with a workspace carry a placeholder drive.
        hasher.update([u8::from(!spec.mounts.is_empty())]);
        hasher.update(spec.vcpus.to_le_bytes());
        hasher.update(spec.memory_mib.to_le_bytes());
        hasher.update(format!("{:?}", spec.network));
        hasher.update(c.kernel_args.as_bytes());
        hasher.update(c.dns.join(",").as_bytes());
        hasher.update(c.agent_port.to_le_bytes());
        for arg in &c.template_warmup_argv {
            hasher.update(arg.as_bytes());
            hasher.update([0]);
        }
        hasher.update(cpu_model().unwrap_or_default().as_bytes());
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        let key: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        c.data_dir.join("templates").join(format!("{key}.json"))
    }

    /// Delete all but the `template_limit` most recently used templates.
    pub(crate) async fn prune_templates(&self) {
        let _guard = self.template_lock.lock().await;
        let dir = self.config.data_dir.join("templates");
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            return;
        };
        let mut refs = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let used = entry
                    .metadata()
                    .await
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                refs.push((used, path));
            }
        }
        refs.sort_by_key(|r| std::cmp::Reverse(r.0));
        for (_, path) in refs.into_iter().skip(self.config.template_limit) {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if let Ok(snapshot) = serde_json::from_slice::<SnapshotRef>(&bytes) {
                    let snap_dir = Path::new(&snapshot.engine_ref);
                    if snap_dir.starts_with(self.config.data_dir.join("snapshots")) {
                        let _ = tokio::fs::remove_dir_all(snap_dir).await;
                    }
                }
            }
            let _ = tokio::fs::remove_file(&path).await;
            tracing::info!(template = %path.display(), "pruned VM template");
        }
    }

    pub(crate) async fn cached_template(&self, path: &Path) -> Option<SnapshotRef> {
        let bytes = tokio::fs::read(path).await.ok()?;
        let snapshot: SnapshotRef = serde_json::from_slice(&bytes).ok()?;
        let dir = Path::new(&snapshot.engine_ref);
        if !dir.starts_with(self.config.data_dir.join("snapshots")) {
            return None;
        }
        if snapshot.cpu_model != cpu_model() {
            return None;
        }
        for file in ["vmstate", "mem", "root.ext4", "meta.json"] {
            if !dir.join(file).is_file() {
                return None;
            }
        }
        // Mark as recently used, for pruning.
        if let Ok(f) = std::fs::File::options().write(true).open(path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        Some(snapshot)
    }

    pub(crate) async fn template_for(
        &self,
        spec: &SandboxSpec,
        base: &Path,
    ) -> Result<SnapshotRef> {
        let path = self.template_ref_path(spec, base);
        if let Some(snapshot) = self.cached_template(&path).await {
            return Ok(snapshot);
        }
        let _guard = self.template_lock.lock().await;
        if let Some(snapshot) = self.cached_template(&path).await {
            return Ok(snapshot);
        }
        let mut blank = spec.clone();
        blank.id = SandboxId(format!("template-{}", Uuid::now_v7().simple()));
        blank.mounts.clear();
        if !spec.mounts.is_empty() {
            // An empty, never-mounted drive in the workspace's slot: a restore
            // puts the real volume at the same drive path (review A4).
            let placeholder = self
                .config
                .data_dir
                .join("templates")
                .join("placeholder.img");
            if !placeholder.exists() {
                tokio::fs::create_dir_all(placeholder.parent().expect("has a parent")).await?;
                let f = tokio::fs::File::create(&placeholder).await?;
                f.set_len(1 << 20).await?;
            }
            blank.mounts.push(vmlayer_core::Mount {
                volume: vmlayer_core::VolumeId("placeholder".into()),
                source: vmlayer_core::VolumeSource::BlockImage { path: placeholder },
                guest_path: String::new(),
                read_only: false,
            });
        }
        blank.egress = None;
        blank.limits = Default::default();
        blank.env.clear();
        blank.workdir = "/".into();
        blank.entrypoint = None;
        blank.ttl = None;
        blank.labels.clear();

        let guest = self.cold_boot(blank, base.to_path_buf()).await?;
        if !self.config.template_warmup_argv.is_empty() {
            let request = ExecRequest {
                argv: self.config.template_warmup_argv.clone(),
                cwd: None,
                env: Default::default(),
                timeout: Some(Duration::from_secs(30)),
                pty: None,
            };
            match guest.exec(request).await {
                Ok(mut process) => match process.wait().await {
                    Ok(status) if status.code == Some(0) => {}
                    other => tracing::warn!(result = ?other, "template warmup did not complete"),
                },
                Err(error) => tracing::warn!(%error, "template warmup could not start"),
            }
        }
        let result = guest
            .snapshot(SnapshotOptions {
                stay_paused: true,
                ..Default::default()
            })
            .await;
        let killed = guest.kill().await;
        let snapshot = result?;
        killed?;

        let parent = path.parent().expect("template path has a parent");
        tokio::fs::create_dir_all(parent).await?;
        let temp = parent.join(format!(".{}.partial", Uuid::now_v7()));
        let bytes = serde_json::to_vec(&snapshot).map_err(|e| crate::api::err(e.to_string()))?;
        tokio::fs::write(&temp, bytes).await?;
        tokio::fs::rename(&temp, &path).await?;
        tracing::info!(snapshot = %snapshot.id, "built data-free VM template");
        drop(_guard);
        self.prune_templates().await;
        Ok(snapshot)
    }
}
