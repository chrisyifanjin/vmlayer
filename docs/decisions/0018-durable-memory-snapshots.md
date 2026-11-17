# 0018. Durable memory snapshots

Status: accepted, 2026-11-17

## Context

ADR 0011 made volume snapshots durable and deliberately left memory
snapshots local: they restore only on the same Firecracker release and CPU
model, so they are less portable. Within a fleet of identical hosts they
are still the fastest way to move or resume a running agent, processes and
all, and rescheduling (ADR 0015) loses memory state without them.

## Decision

- `Sandbox.snapshot(durable=True)` takes the snapshot as before, makes its
  volume snapshots durable, and pushes every file of the backend's snapshot
  directory (Firecracker: `vmstate`, `mem`, `root.ext4`, `meta.json`) as a
  block image into the same chunk store, under
  `sandbox-snapshot-files/<id>/`. All-zero memory is not stored, and
  memory pages that did not change between snapshots are not uploaded
  again. A record at `sandbox-snapshots/<id>.json` holds the `SnapshotRef`
  and the file list, written last. If any upload fails, no snapshot is kept.
- `Sandbox.restore(id)` on a daemon that does not have the snapshot fetches
  the record, downloads the files into its own snapshot directory,
  registers the snapshot, and restores normally; the volumes restore lazily
  (ADR 0016). The backend refuses an incompatible CPU model or release.
- `Snapshot.delete(local_only=True)` frees the local copy; a full delete
  removes the record, file manifests and volume snapshots from the store.
  `list_snapshots(remote=True)` lists the store's durable snapshots.
- Garbage collection treats sandbox snapshot files as references.
- Backends whose snapshots are not files under the data directory (Docker
  commits images) refuse `durable=True`.

## Consequences

- A running sandbox can be resumed on another host of the same type with
  its processes; tested on KVM by deleting the local copy and restoring a
  sandbox whose counter process kept counting.
- Memory files are as large as guest memory before zero elision; the
  upload costs scale with the memory the guest actually used.
- Memory snapshots are not lazy: the files are downloaded before the
  restore, unlike volumes.
