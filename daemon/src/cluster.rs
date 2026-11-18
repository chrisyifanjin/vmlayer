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
//! Single-instance sandboxes (review A2). Each daemon renews **one** lease
//! for its host, `leases/host-<host id>.json`, every third of its TTL; that
//! is the only periodic write, so store traffic scales with hosts, not
//! sandboxes. A sandbox created with `lease = "<name>"` claims an ownership
//! record, `owners/<name>.json` = {host, sandbox, epoch}, by
//! compare-and-swap. A record can be taken over only when it was released
//! or its host's lease has expired, and every takeover bumps the epoch, so a
//! stale owner's conditional writes fail. After each host-lease renewal the
//! daemon pushes the new expiry to the shims of its leased sandboxes as a
//! fencing deadline; if renewals stop (daemon dead, store unreachable), each
//! shim kills its VM at that deadline, before anyone else may take the
//! name over.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vmlayer_core::SandboxId;

use crate::manager::Daemon;

pub const OWNERS_DIR: &str = "owners";

/// Who runs a single-instance sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub name: String,
    pub host: String,
    pub sandbox_id: String,
    /// Bumped by every claim; writes about the sandbox check it.
    pub epoch: u64,
    #[serde(default)]
    pub released: bool,
}

/// Host-lease state shared by the daemon's tasks.
#[derive(Default)]
pub struct ClusterState {
    /// When this host's lease is known to expire.
    pub host_valid_until: std::sync::Mutex<Option<SystemTime>>,
    /// Epoch of each local single-instance sandbox.
    pub epochs: std::sync::Mutex<std::collections::HashMap<SandboxId, u64>>,
}

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
    /// What this host admits in total (0 = unlimited), per its own config.
    #[serde(default)]
    pub allocatable_vcpus: u32,
    #[serde(default)]
    pub allocatable_memory_mib: u64,
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

impl Daemon {
    pub fn host_id(&self) -> String {
        self.cfg
            .cluster
            .host_id
            .clone()
            .map(|h| sanitize(&h))
            .unwrap_or_else(default_host_id)
    }

    fn lease_ttl(&self) -> Duration {
        Duration::from_secs(self.cfg.cluster.lease_ttl_secs.max(3))
    }

    fn host_lease_name(&self) -> String {
        format!("host-{}", self.host_id())
    }

    fn host_lease_valid(&self) -> bool {
        self.cluster
            .host_valid_until
            .lock()
            .unwrap()
            .is_some_and(|t| t > SystemTime::now())
    }

    /// Renew this host's lease once, bounded by a third of its TTL. Returns
    /// the new expiry.
    async fn renew_host_lease(&self) -> vmlayer_core::Result<SystemTime> {
        let remote = self
            .volumes
            .remote()
            .ok_or_else(|| vmlayer_core::Error::Guest("no blob store is configured".into()))?;
        let ttl = self.lease_ttl();
        let started = SystemTime::now();
        let (name, holder) = (self.host_lease_name(), self.host_id());
        let attempt = remote.acquire_lease(&name, &holder, ttl);
        match tokio::time::timeout(ttl / 3, attempt).await {
            Ok(Ok(Ok(_))) => {
                let until = started + ttl;
                *self.cluster.host_valid_until.lock().unwrap() = Some(until);
                Ok(until)
            }
            Ok(Ok(Err(e))) => Err(vmlayer_core::Error::Guest(format!(
                "host lease {} is {e} (two daemons with the same host id?)",
                self.host_lease_name()
            ))),
            Ok(Err(e)) => Err(vmlayer_core::Error::Guest(format!("host lease: {e}"))),
            Err(_) => Err(vmlayer_core::Error::Guest(
                "host lease renewal timed out".into(),
            )),
        }
    }

    /// Keep this host's lease and push its expiry to every leased sandbox.
    pub(crate) fn start_host_lease(self: &Arc<Self>) {
        if self.volumes.remote().is_none() {
            return;
        }
        let weak = Arc::downgrade(self);
        let every = self.lease_ttl() / 3;
        tokio::spawn(async move {
            loop {
                let Some(d) = weak.upgrade() else { return };
                match d.renew_host_lease().await {
                    Ok(until) => d.push_deadlines(until).await,
                    Err(e) => {
                        tracing::warn!(error = %e, "host lease renewal failed");
                        if !d.host_lease_valid() {
                            d.fence_without_shim().await;
                        }
                    }
                }
                drop(d);
                tokio::time::sleep(every).await;
            }
        });
    }

    /// Tell each leased sandbox's backend when to kill it if no renewal follows.
    async fn push_deadlines(&self, until: SystemTime) {
        let leased: Vec<SandboxId> = self
            .cluster
            .epochs
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for id in leased {
            if let Ok(live) = self.live(&id).await {
                match live.handle.set_fencing_deadline(Some(until)).await {
                    Ok(()) | Err(vmlayer_core::Error::Unsupported { .. }) => {}
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "could not move the fencing deadline")
                    }
                }
            }
        }
    }

    /// The host lease lapsed while the daemon runs: backends that cannot
    /// fence on their own (Docker) are fenced here.
    async fn fence_without_shim(&self) {
        let leased: Vec<SandboxId> = self
            .cluster
            .epochs
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for id in leased {
            if let Ok(live) = self.live(&id).await {
                let fences_itself = self
                    .registry
                    .get(&live.backend)
                    .is_some_and(|e| e.backend.capabilities().fencing);
                if !fences_itself {
                    tracing::error!(%id, "host lease expired; killing the sandbox to keep its name exclusive");
                    live.fenced
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = self.kill(&id).await;
                }
            }
        }
    }

    async fn owner(
        &self,
        name: &str,
    ) -> vmlayer_core::Result<Option<(Owner, vmlayer_blob::Version)>> {
        let remote = self.volumes.remote().ok_or_else(|| {
            vmlayer_core::Error::Guest(
                "single-instance sandboxes need a blob store (set [blob_store] url)".into(),
            )
        })?;
        let doc = remote
            .get_versioned(OWNERS_DIR, name)
            .await
            .map_err(|e| vmlayer_core::Error::Guest(format!("owner record {name}: {e}")))?;
        Ok(doc.and_then(|(b, v)| serde_json::from_slice::<Owner>(&b).ok().map(|o| (o, v))))
    }

    /// Whether `host` has stopped renewing its lease.
    async fn host_expired(&self, host: &str) -> bool {
        let Some(remote) = self.volumes.remote() else {
            return false;
        };
        match remote.lease(&format!("host-{host}")).await {
            Ok(Some(l)) => l.expires_at <= now_ms(),
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Whether the name is free for a new owner (released, or its host dead).
    pub(crate) async fn owner_claimable(&self, name: &str) -> bool {
        match self.owner(name).await {
            Ok(None) => true,
            Ok(Some((o, _))) => o.released || self.host_expired(&o.host).await,
            Err(_) => false,
        }
    }

    /// Claim `name` for sandbox `id` on this host. Returns the new epoch.
    pub(crate) async fn acquire_lease(
        &self,
        name: &str,
        id: &SandboxId,
    ) -> vmlayer_core::Result<()> {
        if !self.host_lease_valid() {
            self.renew_host_lease().await?;
        }
        let remote = self
            .volumes
            .remote()
            .expect("checked by renew_host_lease")
            .clone();
        for _ in 0..4 {
            let current = self.owner(name).await?;
            let (epoch, expected) = match &current {
                None => (1, None),
                Some((o, v)) => {
                    let mine_but_gone = o.host == self.host_id()
                        && self.live(&SandboxId(o.sandbox_id.clone())).await.is_err();
                    if !(o.released || mine_but_gone || self.host_expired(&o.host).await) {
                        return Err(vmlayer_core::Error::Guest(format!(
                            "lease {name:?} is held by {}/{} (epoch {})",
                            o.host, o.sandbox_id, o.epoch
                        )));
                    }
                    (o.epoch + 1, Some(v.clone()))
                }
            };
            let record = Owner {
                name: name.to_string(),
                host: self.host_id(),
                sandbox_id: id.0.clone(),
                epoch,
                released: false,
            };
            let bytes = serde_json::to_vec(&record)
                .map_err(|e| vmlayer_core::Error::Guest(e.to_string()))?;
            match remote.put_if(OWNERS_DIR, name, &bytes, expected).await {
                Ok(true) => {
                    self.cluster
                        .epochs
                        .lock()
                        .unwrap()
                        .insert(id.clone(), epoch);
                    return Ok(());
                }
                Ok(false) => continue, // lost a race; look again
                Err(e) => return Err(vmlayer_core::Error::Guest(format!("lease {name:?}: {e}"))),
            }
        }
        Err(vmlayer_core::Error::Guest(format!(
            "lease {name:?}: too much contention"
        )))
    }

    /// The epoch this sandbox owns `name` at, if it still does.
    pub(crate) async fn owned_epoch(&self, name: &str, id: &SandboxId) -> Option<u64> {
        let epoch = *self.cluster.epochs.lock().unwrap().get(id)?;
        match self.owner(name).await {
            Ok(Some((o, _)))
                if o.host == self.host_id()
                    && o.sandbox_id == id.0
                    && o.epoch == epoch
                    && !o.released =>
            {
                Some(epoch)
            }
            _ => None,
        }
    }

    /// Give `name` up if sandbox `id` still owns it.
    pub(crate) async fn release_lease(&self, name: &str, id: &SandboxId) {
        let epoch = self.cluster.epochs.lock().unwrap().remove(id);
        let (Some(remote), Some(epoch)) = (self.volumes.remote(), epoch) else {
            return;
        };
        if let Ok(Some((mut o, v))) = self.owner(name).await {
            if o.host == self.host_id() && o.sandbox_id == id.0 && o.epoch == epoch {
                o.released = true;
                if let Ok(bytes) = serde_json::to_vec(&o) {
                    let _ = remote.put_if(OWNERS_DIR, name, &bytes, Some(v)).await;
                }
            }
        }
    }

    /// After a restart: keep owning `name` if the record still says so;
    /// otherwise the sandbox must go (someone took the name over).
    pub(crate) async fn resume_ownership(&self, name: &str, id: &SandboxId) -> bool {
        match self.owner(name).await {
            Ok(Some((o, _))) if o.host == self.host_id() && o.sandbox_id == id.0 && !o.released => {
                self.cluster
                    .epochs
                    .lock()
                    .unwrap()
                    .insert(id.clone(), o.epoch);
                true
            }
            Ok(_) => false,
            Err(_) => true, // store unreachable: the host lease decides
        }
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
            memory_mib: crate::admission::host_memory_mib(),
            allocated_vcpus: vcpus,
            allocated_memory_mib: memory,
            allocatable_vcpus: self.allocatable().0,
            allocatable_memory_mib: self.allocatable().1,
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

    /// Pick a host for a new sandbox (review A3): fresh heartbeat, the
    /// backend, matching labels, room for the request by the host's own
    /// admission limits; most free memory first, then fewest sandboxes.
    /// The chosen host still admits or refuses the create itself.
    pub async fn place(
        &self,
        backend: &str,
        vcpus: u32,
        memory_mib: u64,
        placement: &BTreeMap<String, String>,
        exclude: &[String],
    ) -> vmlayer_core::Result<HostDoc> {
        let hosts = self.list_hosts().await?;
        let fits = |h: &HostDoc| {
            let backend_ok = if backend.is_empty() || backend == "auto" {
                !h.backends.is_empty()
            } else {
                h.backends.iter().any(|b| b == backend)
            };
            let free_mem = h
                .allocatable_memory_mib
                .saturating_sub(h.allocated_memory_mib);
            let free_cpu = h.allocatable_vcpus.saturating_sub(h.allocated_vcpus);
            backend_ok
                && !exclude.contains(&h.host_id)
                && placement.iter().all(|(k, v)| h.labels.get(k) == Some(v))
                && (h.allocatable_memory_mib == 0 || free_mem >= memory_mib)
                && (h.allocatable_vcpus == 0 || free_cpu >= vcpus)
        };
        hosts
            .into_iter()
            .filter(fits)
            .max_by_key(|h| {
                let free = if h.allocatable_memory_mib == 0 {
                    u64::MAX / 2
                } else {
                    h.allocatable_memory_mib.saturating_sub(h.allocated_memory_mib)
                };
                (free, std::cmp::Reverse(h.sandboxes.len()))
            })
            .ok_or_else(|| {
                vmlayer_core::Error::ResourceExhausted(format!(
                    "no host offers backend {backend:?} with {memory_mib} MiB and {vcpus} vCPUs free{}",
                    if placement.is_empty() { String::new() } else { format!(" and labels {placement:?}") }
                ))
            })
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
