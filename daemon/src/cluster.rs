//! Several hosts, one blob store: host registry and single-instance leases
//! (decision 0014).
//!
//! Each daemon with a blob store and an `advertise` address writes a
//! heartbeat document to `<store>/hosts/<host id>.json` every few seconds:
//! its endpoint, backends, capacity, what its sandboxes have allocated, and
//! their ids. Clients list the fresh documents (`System.ListHosts`) and
//! place new sandboxes themselves; the SDK's `Cluster` does that. There is no
//! central scheduler to run or keep available.
//!
//! A sandbox created with `lease = "<name>"` holds the store lease `<name>`
//! for as long as it lives, so at most one sandbox with that name runs
//! across all hosts. The daemon renews the lease every third of its TTL. If
//! the lease is lost, or cannot be renewed before it expires (the host is cut
//! off from the store), the daemon kills the sandbox: by the time anyone else
//! can take the lease, this one is gone.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vmlayer_core::SandboxId;

use crate::manager::Daemon;

pub const HOSTS_DIR: &str = "hosts";
/// Label recording a sandbox's lease, so renewal resumes after a restart.
pub const LEASE_LABEL: &str = "vmlayer.lease";

/// One host's heartbeat.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDoc {
    pub host_id: String,
    pub endpoint: String,
    pub daemon_version: String,
    /// Backends usable on this host.
    pub backends: Vec<String>,
    pub cpus: u32,
    /// 0 when unknown.
    pub memory_mib: u64,
    pub allocated_vcpus: u32,
    pub allocated_memory_mib: u64,
    pub sandboxes: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Milliseconds since the Unix epoch.
    pub updated_at: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `name` reduced to the characters a store key allows.
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() {
        "host".into()
    } else {
        s
    }
}

pub fn default_host_id() -> String {
    let name = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_default();
    sanitize(name.trim())
}

fn host_memory_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map(|kib| kib / 1024)
        .unwrap_or(0)
}

impl Daemon {
    pub fn host_id(&self) -> String {
        self.cfg
            .cluster
            .host_id
            .clone()
            .map(|h| sanitize(&h))
            .unwrap_or_else(default_host_id)
    }

    pub(crate) fn lease_holder(&self, id: &SandboxId) -> String {
        format!("{}/{}", self.host_id(), id)
    }

    fn lease_ttl(&self) -> Duration {
        Duration::from_secs(self.cfg.cluster.lease_ttl_secs.max(3))
    }

    async fn host_doc(&self, endpoint: &str) -> HostDoc {
        let (mut vcpus, mut memory, mut ids) = (0u32, 0u64, Vec::new());
        for row in self.store.list_sandboxes().unwrap_or_default() {
            if self.live(&row.id).await.is_ok() {
                vcpus += row.spec.vcpus;
                memory += u64::from(row.spec.memory_mib);
                ids.push(row.id.0.clone());
            }
        }
        HostDoc {
            host_id: self.host_id(),
            endpoint: endpoint.to_string(),
            daemon_version: crate::VERSION.to_string(),
            backends: self
                .registry
                .entries()
                .filter(|(_, e)| e.unavailable.is_none())
                .map(|(n, _)| n.clone())
                .collect(),
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            memory_mib: host_memory_mib(),
            allocated_vcpus: vcpus,
            allocated_memory_mib: memory,
            sandboxes: ids,
            labels: self.cfg.cluster.labels.clone(),
            updated_at: now_ms(),
        }
    }

    /// Publish this host's heartbeat until the daemon is dropped.
    pub(crate) fn start_heartbeat(self: &Arc<Self>) {
        let (Some(remote), Some(endpoint)) = (
            self.volumes.remote().cloned(),
            self.cfg.cluster.advertise.clone(),
        ) else {
            return;
        };
        let every = Duration::from_secs(self.cfg.cluster.heartbeat_secs.max(1));
        let weak: Weak<Daemon> = Arc::downgrade(self);
        tracing::info!(host = %self.host_id(), %endpoint, "publishing host heartbeat");
        tokio::spawn(async move {
            loop {
                let Some(d) = weak.upgrade() else { return };
                let doc = d.host_doc(&endpoint).await;
                let host = doc.host_id.clone();
                drop(d);
                match serde_json::to_vec(&doc) {
                    Ok(bytes) => {
                        if let Err(e) = remote.put_doc(HOSTS_DIR, &host, &bytes).await {
                            tracing::warn!(error = %e, "could not publish host heartbeat");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "host heartbeat"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }

    /// Remove this host's heartbeat (clean shutdown).
    pub async fn withdraw_heartbeat(&self) {
        if let (Some(remote), Some(_)) = (self.volumes.remote(), &self.cfg.cluster.advertise) {
            let _ = remote.delete_doc(HOSTS_DIR, &self.host_id()).await;
        }
    }

    /// Fresh heartbeats of every host sharing the blob store.
    pub async fn list_hosts(&self) -> vmlayer_core::Result<Vec<HostDoc>> {
        let remote = self.volumes.remote().ok_or_else(|| {
            vmlayer_core::Error::Guest(
                "no blob store is configured, so there is no host registry".into(),
            )
        })?;
        let stale = 3 * 1000 * self.cfg.cluster.heartbeat_secs.max(1);
        let docs = remote
            .list_docs(HOSTS_DIR)
            .await
            .map_err(|e| vmlayer_core::Error::Engine {
                backend: "blob-store",
                message: e.to_string(),
                source: None,
            })?;
        let now = now_ms();
        let mut hosts: Vec<HostDoc> = docs
            .iter()
            .filter_map(|b| serde_json::from_slice::<HostDoc>(b).ok())
            .filter(|h| now.saturating_sub(h.updated_at) <= stale)
            .collect();
        hosts.sort_by(|a, b| a.host_id.cmp(&b.host_id));
        Ok(hosts)
    }

    /// Take the lease for a sandbox about to be created.
    pub(crate) async fn acquire_lease(
        &self,
        name: &str,
        id: &SandboxId,
    ) -> vmlayer_core::Result<()> {
        let remote = self.volumes.remote().ok_or_else(|| {
            vmlayer_core::Error::Guest("leases need a blob store (set [blob_store] url)".into())
        })?;
        match remote
            .acquire_lease(name, &self.lease_holder(id), self.lease_ttl())
            .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(vmlayer_core::Error::Guest(format!("lease {name:?} is {e}"))),
            Err(e) => Err(vmlayer_core::Error::Guest(format!("lease {name:?}: {e}"))),
        }
    }

    pub(crate) async fn release_lease(&self, name: &str, id: &SandboxId) {
        if let Some(remote) = self.volumes.remote() {
            if let Err(e) = remote.release_lease(name, &self.lease_holder(id)).await {
                tracing::warn!(lease = name, error = %e, "could not release lease");
            }
        }
    }

    /// Keep the lease while the sandbox lives; kill the sandbox if it goes.
    pub(crate) fn keep_lease(
        self: &Arc<Self>,
        id: SandboxId,
        name: String,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let remote = self.volumes.remote().cloned()?;
        let ttl = self.lease_ttl();
        let holder = self.lease_holder(&id);
        let weak = Arc::downgrade(self);
        Some(tokio::spawn(async move {
            let mut valid_until = Instant::now() + ttl;
            loop {
                let started = Instant::now();
                let outcome = remote.renew_lease(&name, &holder, ttl).await;
                let Some(d) = weak.upgrade() else { return };
                let reason = match outcome {
                    Ok(Ok(_)) => {
                        valid_until = started + ttl;
                        None
                    }
                    Ok(Err(e)) => Some(format!("lease {name:?} {e}")),
                    Err(e) if Instant::now() >= valid_until => Some(format!(
                        "lease {name:?} could not be renewed before it expired: {e}"
                    )),
                    Err(e) => {
                        tracing::warn!(lease = %name, error = %e, "lease renewal failed; retrying");
                        None
                    }
                };
                if let Some(reason) = reason {
                    tracing::error!(%id, %reason, "killing sandbox to keep the lease exclusive");
                    if let Ok(live) = d.live(&id).await {
                        live.fenced
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Err(e) = d.kill(&id).await {
                        tracing::warn!(%id, error = %e, "kill after losing the lease failed");
                    }
                    return;
                }
                drop(d);
                tokio::time::sleep(ttl / 3).await;
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ids_are_store_safe() {
        assert_eq!(
            sanitize("ip-10-0-0-1.ec2.internal"),
            "ip-10-0-0-1-ec2-internal"
        );
        assert_eq!(sanitize(""), "host");
        assert!(sanitize(&"x".repeat(200)).len() <= 64);
    }
}
