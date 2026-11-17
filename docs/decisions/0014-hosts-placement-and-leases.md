# 0014. Several hosts: a registry and leases in the blob store, placement in the client

Status: accepted, 2026-11-17

## Context

One daemon runs one host. Platforms with more load than a host need three
things that every team otherwise writes itself:

1. **Know the hosts**: which daemons exist, how to reach them, what they
   can run and how full they are.
2. **Place** a new sandbox on a suitable host.
3. **Never run the same workload twice**: an agent's workspace must not be
   live on two hosts at once, even when a host is partitioned from the rest.

ADR 0005 rules out required managed services and Kubernetes. ADR 0011 gave
every multi-host deployment an object store already.

## Options considered

**A scheduler service with its own database** (Nomad- or Kubernetes-style
control plane). Full featured, but a second component to deploy, make
highly available and secure, which is exactly what "no Kubernetes" users
want to avoid.

**A consensus store** (etcd, Consul) for membership and locks. Correct and
well understood; again a cluster to run.

**The object store as the coordination medium.** S3 (since 2024), Azure
Blob and GCS all offer conditional writes: create-if-absent and
compare-and-swap on the object version. That is enough for leases with
expiry (the pattern of Delta Lake's and Iceberg's commit protocols, and of
lock tables in many backup tools). Membership is a heartbeat object per host
with a freshness window. Chosen: no new component.

**Where to place.** A placement decision needs the heartbeats and a policy.
Doing it in the client (SDK) avoids a service and keeps policies in the
platform's hands; the daemon only publishes facts.

## Decision

- **Heartbeats.** A daemon with a blob store and `[cluster] advertise`
  writes `hosts/<host_id>.json` every `heartbeat_secs` (10): endpoint,
  version, usable backends, CPUs, memory, totals allocated by its running
  sandboxes, their ids, and operator labels. It deletes the document on
  clean shutdown. `System.ListHosts` returns documents fresher than three
  heartbeats.
- **Placement** is in the SDK: `vmlayer.Cluster(seed)` lists hosts through
  any daemon, filters by backend, labels (`placement={"zone": "a"}`) and free
  memory, and picks the most free memory, then the fewest sandboxes.
  `Cluster.connect(id)` finds the host running a sandbox.
- **Leases.** `leases/<name>.json` holds `{holder, expires_at}`. Acquire is a
  create-if-absent write; renewal and takeover of an expired lease are
  compare-and-swap writes on the object version; only the holder deletes.
  A store without compare-and-swap (`file://`) falls back to
  delete-then-create, which is crash-safe but can admit two takers racing
  within milliseconds; documented as development only.
- **Single-instance sandboxes.** `Sandbox.create(..., lease="repo-42")` takes
  the lease before booting, labels the sandbox with it, renews it every third
  of `lease_ttl_secs` (30), resumes renewal after a daemon restart, and
  releases it on kill. **Fencing:** if the lease is lost, or cannot be
  renewed before it expires (the host lost the store), the daemon kills the
  sandbox. Since another holder can only take the lease after expiry, and
  the old holder stops at expiry at the latest, two sandboxes with one lease
  never run at the same time, as long as host clocks agree within a small
  fraction of the TTL.

## Consequences

- No scheduler or database to operate; the blob store becomes the one
  shared dependency of a multi-host deployment.
- Placement sees state up to one heartbeat old, so bursts can overshoot a
  host's capacity. Daemons still enforce their own limits (admission control,
  ADR 0008 addendum), and callers retry on another host.
- Lease safety depends on clocks: expiry is wall-clock time written by the
  holder and judged by the taker. NTP-synchronised hosts and a TTL of tens
  of seconds leave a wide margin.
- A sandbox's data is still local to its host. Moving a workload means a
  durable snapshot (ADR 0011) restored elsewhere, under the same lease.
- Rescheduling sandboxes from a dead host automatically is not done; the
  lease makes it safe for a platform to do it.
