//! Automatic rescheduling of single-instance sandboxes (decision 0015).
//!
//! A sandbox created with `lease = "<name>"` and `reschedule = true` gets a
//! workload record at `<store>/workloads/<name>.json`: how to create it again
//! (image, resources, environment, entrypoint, policy) and the id of its
//! latest workspace checkpoint. Its daemon uploads a durable snapshot of the
//! workspace every `checkpoint_secs` and points the record at it.
//!
//! Daemons with `[cluster] rescue = true` scan the records. When a record's
//! lease has expired, its host is gone (a live host renews every third of
//! the TTL, and kills the sandbox when it cannot). The scanner downloads the
//! checkpoint into a new volume and creates the sandbox with the same lease;
//! the lease's compare-and-swap lets exactly one scanner win. The new sandbox
//! boots fresh on the checkpointed files: anything written after the last
//! checkpoint, and all memory state, is lost.
//!
//! Killing a rescheduled sandbox deletes its record and checkpoint, so it is
//! not brought back. A sandbox killed for losing its lease keeps both: the
//! new holder owns them.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use vmlayer_core::{Error, IoLimits, NetworkMode, Result, SandboxId, VolumeSnapshotId};

use crate::cluster::{now_ms, LEASE_LABEL};
use crate::manager::{CreateParams, Daemon, EntrypointSpec, WorkspaceSpec};

pub const WORKLOADS_DIR: &str = "workloads";
/// Marks a sandbox as rescheduled; its value is the lease name.
pub const RESCHEDULE_LABEL: &str = "vmlayer.reschedule";
/// Labels checkpoint snapshots with their workload.
pub const CHECKPOINT_LABEL: &str = "vmlayer.workload";

/// Everything needed to create the sandbox again elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    pub name: String,
    pub image: String,
    pub backend: String,
    pub vcpus: Option<u32>,
    pub memory_mib: Option<u32>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    /// `None`: the image's entrypoint; `Some([])`: none; else this argv.
    pub entrypoint: Option<Vec<String>>,
    pub network: Option<NetworkMode>,
    pub egress: Option<Vec<String>>,
    pub limits: IoLimits,
    pub labels: BTreeMap<String, String>,
    /// Whether the sandbox had a workspace volume to checkpoint.
    pub workspace: bool,
    /// Durable volume snapshot of the workspace, once one was taken.
    pub checkpoint: Option<String>,
    pub host: String,
    pub sandbox_id: String,
    pub updated_at: u64,
}

impl Workload {
    pub fn from_params(name: &str, p: &CreateParams) -> Result<Self> {
        if !p.mounts.is_empty() {
            return Err(Error::Guest(
                "reschedule supports the workspace volume only, not extra mounts".into(),
            ));
        }
        if matches!(p.workspace, WorkspaceSpec::Volume(_)) {
            return Err(Error::Guest(
                "reschedule needs a workspace the daemon owns (the default); a named volume stays on its host".into(),
            ));
        }
        Ok(Self {
            name: name.to_string(),
            image: p.image.clone(),
            backend: p.backend.clone(),
            vcpus: p.vcpus,
            memory_mib: p.memory_mib,
            env: p.env.clone(),
            workdir: p.workdir.clone(),
            entrypoint: match &p.entrypoint {
                EntrypointSpec::ImageDefault => None,
                EntrypointSpec::None => Some(Vec::new()),
                EntrypointSpec::Argv(a) => Some(a.clone()),
            },
            network: p.network.clone(),
            egress: p
                .egress
                .as_ref()
                .map(|e| e.allow.iter().map(|r| r.to_string()).collect()),
            limits: p.limits,
            labels: p.labels.clone(),
            workspace: matches!(p.workspace, WorkspaceSpec::Anonymous),
            checkpoint: None,
            host: String::new(),
            sandbox_id: String::new(),
            updated_at: now_ms(),
        })
    }

    fn to_params(&self, workspace: Option<String>) -> Result<CreateParams> {
        Ok(CreateParams {
            image: self.image.clone(),
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            disk_mib: None,
            env: self.env.clone(),
            workdir: self.workdir.clone(),
            workspace: match (self.workspace, workspace) {
                (_, Some(v)) => WorkspaceSpec::Volume(v),
                (true, None) => WorkspaceSpec::Anonymous,
                (false, None) => WorkspaceSpec::None,
            },
            mounts: Vec::new(),
            entrypoint: match &self.entrypoint {
                None => EntrypointSpec::ImageDefault,
                Some(a) if a.is_empty() => EntrypointSpec::None,
                Some(a) => EntrypointSpec::Argv(a.clone()),
            },
            ttl: None,
            backend: self.backend.clone(),
            network: self.network.clone(),
            labels: self.labels.clone(),
            egress: self
                .egress
                .as_ref()
                .map(|r| vmlayer_core::EgressPolicy::parse(r))
                .transpose()
                .map_err(Error::Guest)?,
            limits: self.limits,
            lease: Some(self.name.clone()),
            reschedule: true,
            rescued: Some(Box::new(self.clone())),
        })
    }
}

fn blob(e: vmlayer_blob::Error) -> Error {
    Error::Engine {
        backend: "blob-store",
        message: e.to_string(),
        source: None,
    }
}

impl Daemon {
    async fn read_workload(&self, name: &str) -> Result<Option<Workload>> {
        let remote = self
            .volumes
            .remote()
            .ok_or_else(|| Error::Guest("no blob store".into()))?;
        for doc in remote.list_docs(WORKLOADS_DIR).await.map_err(blob)? {
            if let Ok(w) = serde_json::from_slice::<Workload>(&doc) {
                if w.name == name {
                    return Ok(Some(w));
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn write_workload(&self, w: &Workload) -> Result<()> {
        let remote = self
            .volumes
            .remote()
            .ok_or_else(|| Error::Guest("no blob store".into()))?;
        let bytes = serde_json::to_vec(w).map_err(|e| Error::Guest(e.to_string()))?;
        remote
            .put_doc(WORKLOADS_DIR, &w.name, &bytes)
            .await
            .map_err(blob)
    }

    /// The sandbox was killed on purpose: forget the workload and its checkpoint.
    pub(crate) async fn forget_workload(&self, name: &str, id: &SandboxId) {
        let Ok(Some(w)) = self.read_workload(name).await else {
            return;
        };
        if w.sandbox_id != id.0 {
            return; // someone else's now
        }
        if let Some(c) = &w.checkpoint {
            let _ = self
                .volumes
                .delete_snapshot(&VolumeSnapshotId(c.clone()), false)
                .await;
        }
        if let Some(remote) = self.volumes.remote() {
            let _ = remote.delete_doc(WORKLOADS_DIR, name).await;
        }
    }

    /// Upload a durable snapshot of the workspace every `checkpoint_secs`.
    pub(crate) fn keep_checkpoints(
        self: &Arc<Self>,
        id: SandboxId,
        name: String,
    ) -> tokio::task::JoinHandle<()> {
        let every = Duration::from_secs(self.cfg.cluster.checkpoint_secs.max(1));
        let weak: Weak<Daemon> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(d) = weak.upgrade() else { return };
                if let Err(e) = d.checkpoint(&id, &name).await {
                    tracing::warn!(%id, workload = %name, error = %e, "checkpoint failed");
                }
            }
        })
    }

    async fn checkpoint(&self, id: &SandboxId, name: &str) -> Result<()> {
        let live = self.live(id).await?;
        let row = self
            .store
            .get_sandbox(id)
            .map_err(|e| Error::Guest(e.to_string()))?;
        let Some(volume) = row.workspace_volume_id.clone() else {
            return Ok(());
        };
        // Only the current owner (this epoch) may move the record forward.
        if self.owned_epoch(name, id).await.is_none() {
            return Ok(());
        }
        let _ = live.handle.freeze_fs().await;
        let labels = BTreeMap::from([(CHECKPOINT_LABEL.to_string(), name.to_string())]);
        let snap = self
            .volumes
            .snapshot(&volume, Some(row.workdir.clone()), labels, true)
            .await;
        let _ = live.handle.thaw_fs().await;
        let snap = snap?;
        // The blob store has it; free the local disk.
        let _ = self.volumes.delete_snapshot(&snap.id, true).await;
        let mut w = match self.read_workload(name).await? {
            Some(w) => w,
            None => return Ok(()), // forgotten meanwhile
        };
        let previous = w.checkpoint.replace(snap.id.0.clone());
        w.host = self.host_id();
        w.sandbox_id = id.0.clone();
        w.updated_at = now_ms();
        self.write_workload(&w).await?;
        if let Some(prev) = previous {
            let _ = self
                .volumes
                .delete_snapshot(&VolumeSnapshotId(prev), false)
                .await;
        }
        tracing::debug!(%id, workload = %name, snapshot = %snap.id, "checkpointed");
        Ok(())
    }

    /// Scan for workloads whose host died and bring them up here.
    pub(crate) fn start_rescue(self: &Arc<Self>) {
        if !self.cfg.cluster.rescue {
            return;
        }
        let Some(remote) = self.volumes.remote().cloned() else {
            tracing::warn!("[cluster] rescue needs a blob store; not rescuing");
            return;
        };
        let every = Duration::from_secs(2 * self.cfg.cluster.heartbeat_secs.max(1));
        let weak: Weak<Daemon> = Arc::downgrade(self);
        tracing::info!("rescuing workloads of failed hosts");
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(d) = weak.upgrade() else { return };
                let docs = match remote.list_docs(WORKLOADS_DIR).await {
                    Ok(docs) => docs,
                    Err(e) => {
                        tracing::warn!(error = %e, "listing workloads");
                        continue;
                    }
                };
                for w in docs
                    .iter()
                    .filter_map(|b| serde_json::from_slice::<Workload>(b).ok())
                {
                    let expired = d.owner_claimable(&w.name).await;
                    let can_run = d
                        .registry
                        .get(&w.backend)
                        .is_some_and(|e| e.unavailable.is_none())
                        || (w.backend.is_empty() || w.backend == "auto");
                    if expired && can_run {
                        d.rescue(w).await;
                    }
                }
            }
        });
    }

    async fn rescue(self: &Arc<Self>, w: Workload) {
        let name = w.name.clone();
        let volume = match &w.checkpoint {
            Some(c) => match self
                .volumes
                .from_snapshot(&VolumeSnapshotId(c.clone()), None, BTreeMap::new(), true)
                .await
            {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(workload = %name, error = %e, "cannot restore checkpoint");
                    return;
                }
            },
            None => None,
        };
        let params = match w.to_params(volume.as_ref().map(|v| v.id.0.clone())) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(workload = %name, error = %e, "bad workload record");
                return;
            }
        };
        match self.create(params).await {
            Ok(view) => {
                tracing::info!(workload = %name, id = %view.row.id, from = %w.host, "rescued workload")
            }
            Err(e) => {
                // Usually another daemon won the lease first.
                tracing::debug!(workload = %name, error = %e, "not rescued here");
                if let Some(v) = volume {
                    let _ = self.volumes.delete(&v.id).await;
                }
            }
        }
    }
}

pub(crate) fn lease_name(labels: &BTreeMap<String, String>) -> Option<&String> {
    labels.get(RESCHEDULE_LABEL).and(labels.get(LEASE_LABEL))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> CreateParams {
        CreateParams {
            image: "python:3.12".into(),
            vcpus: Some(2),
            memory_mib: None,
            disk_mib: None,
            env: BTreeMap::from([("A".into(), "1".into())]),
            workdir: None,
            workspace: WorkspaceSpec::Anonymous,
            mounts: Vec::new(),
            entrypoint: EntrypointSpec::Argv(vec!["sleep".into(), "1".into()]),
            ttl: None,
            backend: "docker".into(),
            network: None,
            labels: BTreeMap::new(),
            egress: Some(vmlayer_core::EgressPolicy::parse(&["pypi.org:443"]).unwrap()),
            limits: IoLimits::default(),
            lease: Some("svc".into()),
            reschedule: true,
            rescued: None,
        }
    }

    #[test]
    fn workload_round_trips_to_create_params() {
        let w = Workload::from_params("svc", &params()).unwrap();
        let json = serde_json::to_vec(&w).unwrap();
        let back: Workload = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, w);
        let p = back.to_params(Some("vol-1".into())).unwrap();
        assert!(matches!(p.workspace, WorkspaceSpec::Volume(ref v) if v == "vol-1"));
        assert!(matches!(p.entrypoint, EntrypointSpec::Argv(ref a) if a == &["sleep", "1"]));
        assert_eq!(p.egress.unwrap().allow[0].to_string(), "pypi.org:443");
        assert_eq!((p.lease.as_deref(), p.vcpus), (Some("svc"), Some(2)));
    }

    #[test]
    fn named_workspaces_and_extra_mounts_are_refused() {
        let mut p = params();
        p.workspace = WorkspaceSpec::Volume("mine".into());
        assert!(Workload::from_params("svc", &p).is_err());
    }
}
