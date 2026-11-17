# 0015. Rescheduling sandboxes from failed hosts

Status: accepted, 2026-11-17

## Context

ADR 0014 made workloads single-instance across hosts with leases, and said
a platform could safely bring a dead host's workloads back elsewhere. Every
platform would write that loop itself. The ingredients are in place: leases
expire when a host stops renewing them, and durable volume snapshots
(ADR 0011) can be restored on any host.

## Options considered

**Live migration** (move memory state). Firecracker snapshots are tied to
the CPU model and release, and a dead host cannot be snapshotted. Not
possible for the failure case.

**Replicated volumes** (DRBD, a network filesystem). Zero data loss, but a
storage system to run and a performance cost on every write.

**Periodic checkpoints plus restart.** Upload the workspace on an interval,
recreate the sandbox from the latest upload on failure. The cost is a
recovery point objective of one interval and a cold start. Chosen: it
needs nothing new to operate, and the interval is the operator's choice.

**Who restarts.** A central controller would need high availability. Any
daemon can do it if the lease decides the winner, which it already does.

## Decision

- `Sandbox.create(lease=..., reschedule=True)` writes a workload record,
  `workloads/<lease>.json`, with what is needed to create the sandbox again:
  image, backend, resources, environment, working directory, entrypoint,
  network, egress rules, IO limits and labels. Extra mounts and named
  workspace volumes are refused, because they would not follow the sandbox.
- Every `checkpoint_secs` (60) the holding daemon freezes the workspace,
  takes a durable snapshot, drops the local copy, points the record at it,
  and deletes the previous checkpoint. It only does so while it holds the
  lease.
- Daemons with `[cluster] rescue = true` scan the records every two
  heartbeats. For a record whose lease has expired and whose backend they
  have, they restore the checkpoint into a new volume and create the
  sandbox with the same lease and record. The lease's compare-and-swap
  admits exactly one winner; losers delete their downloaded volume.
- Killing a rescheduled sandbox, or letting its TTL run out, deletes the
  record and checkpoint. A sandbox killed for losing its lease keeps them,
  because the new holder owns them now.

## Consequences

- Recovery point: up to one checkpoint interval of writes is lost, and all
  memory state. The rescued sandbox runs its entrypoint again.
- Recovery time: about one lease TTL, plus one scan interval, plus restoring
  the checkpoint and a cold boot. That is seconds for small workspaces.
- Each checkpoint uploads only changed chunks (ADR 0011), so short
  intervals are affordable.
- Rescued sandboxes get a new id; clients find them by lease label or through
  `Cluster.connect` after looking them up.
