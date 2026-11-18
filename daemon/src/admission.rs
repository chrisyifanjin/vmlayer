//! Admission control: a daemon refuses work its host cannot hold (review A3).
//!
//! Every create reserves its vCPUs and memory before any work is done, and
//! the reservation counts against the host until the sandbox exists, so
//! concurrent creates cannot overshoot. The limits are the host's CPUs and
//! memory (less a reserve) times configurable overcommit ratios; microVM
//! memory is allocated on touch, so some memory overcommit is safe, and
//! vCPUs are time-shared. A refused create fails with `ResourceExhausted`,
//! which placement treats as "try another host".

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use vmlayer_core::{Error, Result};

use crate::manager::Daemon;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AdmissionConfig {
    pub enabled: bool,
    /// Allocatable memory = (host memory - reserve) x this.
    pub memory_overcommit: f64,
    /// Allocatable vCPUs = host CPUs x this.
    pub vcpu_overcommit: f64,
    /// Memory kept for the host itself.
    pub reserved_memory_mib: u64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_overcommit: 1.0,
            vcpu_overcommit: 4.0,
            reserved_memory_mib: 512,
        }
    }
}

/// Resources of creates in flight.
#[derive(Default)]
pub struct Admission {
    inflight: Mutex<(u32, u64)>,
}

/// A reservation; released when dropped.
pub struct Ticket {
    adm: Arc<Admission>,
    vcpus: u32,
    memory_mib: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut f = self.adm.inflight.lock().unwrap();
        f.0 = f.0.saturating_sub(self.vcpus);
        f.1 = f.1.saturating_sub(self.memory_mib);
    }
}

pub fn host_memory_mib() -> u64 {
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

pub fn host_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

impl Daemon {
    /// (vCPUs, MiB) this host admits in total; 0 means unlimited.
    pub fn allocatable(&self) -> (u32, u64) {
        let a = &self.cfg.admission;
        if !a.enabled {
            return (0, 0);
        }
        let mem = host_memory_mib();
        let mem = if mem == 0 {
            0
        } else {
            ((mem.saturating_sub(a.reserved_memory_mib)) as f64 * a.memory_overcommit) as u64
        };
        ((host_cpus() as f64 * a.vcpu_overcommit) as u32, mem)
    }

    /// (vCPUs, MiB) held by running sandboxes.
    pub async fn allocated(&self) -> (u32, u64) {
        let (mut vcpus, mut mem) = (0u32, 0u64);
        for row in self.store.list_sandboxes().unwrap_or_default() {
            if self.live(&row.id).await.is_ok() {
                vcpus += row.spec.vcpus;
                mem += u64::from(row.spec.memory_mib);
            }
        }
        (vcpus, mem)
    }

    /// Reserve resources for a sandbox, or refuse because the host is full.
    pub async fn admit(&self, vcpus: u32, memory_mib: u64) -> Result<Ticket> {
        let (cap_cpu, cap_mem) = self.allocatable();
        let (used_cpu, used_mem) = self.allocated().await;
        let mut inflight = self.admission.inflight.lock().unwrap();
        let (cpu, mem) = (
            used_cpu + inflight.0 + vcpus,
            used_mem + inflight.1 + memory_mib,
        );
        if cap_mem > 0 && mem > cap_mem {
            self.metrics.inc("vmlayer_admission_refusals_total", &[]);
            return Err(Error::ResourceExhausted(format!(
                "host is full: {memory_mib} MiB requested, {} of {cap_mem} allocatable MiB in use",
                used_mem + inflight.1
            )));
        }
        if cap_cpu > 0 && cpu > cap_cpu {
            self.metrics.inc("vmlayer_admission_refusals_total", &[]);
            return Err(Error::ResourceExhausted(format!(
                "host is full: {vcpus} vCPUs requested, {} of {cap_cpu} allocatable in use",
                used_cpu + inflight.0
            )));
        }
        inflight.0 += vcpus;
        inflight.1 += memory_mib;
        Ok(Ticket {
            adm: self.admission.clone(),
            vcpus,
            memory_mib,
        })
    }
}
