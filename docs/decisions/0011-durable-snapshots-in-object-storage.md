# 0011. Durable volume snapshots: content-addressed chunks in object storage

Status: accepted, 2026-11-17

## Context

Until now every snapshot lived on one host's disk. If the host is lost, so
is every workspace on it, and a workspace cannot move to another host. ADR
0005 promised a blob store behind a trait; ADR 0006 D6 already exposed
`durable=True` on volume snapshots without implementing it.

Requirements:

1. Work on every major cloud and on premises: AWS S3 and S3-compatible
   stores (MinIO, Ceph, R2), Azure Blob, Google Cloud Storage, a shared
   directory.
2. Repeat snapshots of one volume must send only what changed. Agent
   workspaces are snapshotted often and change little between snapshots.
3. A snapshot in the store must be complete or absent, never half-written.
4. Storage must not grow forever after snapshots are deleted.
5. Both volume representations: ext4 images (microVM backends) and
   directories (container backends).

## Options considered

**Whole-file upload** (one object per snapshot, multipart). Simple, but
every snapshot re-uploads the full volume, and restore must download all of
it. Fails requirement 2.

**A file-level store such as JuiceFS or a network filesystem.** Gives lazy
reads for free, but adds a metadata service (Redis or SQL) and a FUSE mount
per host: an operational dependency the "Linux + KVM only" rule (ADR 0005)
excludes from the core. A deployment can still put volumes on such a
filesystem.

**Content-addressed chunks plus a manifest.** Used by restic and borg (for
files), by Proxmox Backup Server (fixed chunks for VM images, dynamic chunks
for file archives), by casync/desync, and in spirit by EBS incremental
snapshots (changed blocks only). Deduplication across snapshots and volumes
comes from naming chunks by their hash. Chosen.

**Per-cloud SDKs versus one abstraction.** The Rust `object_store` crate
(Apache Arrow project; used by DataFusion, delta-rs and InfluxDB 3) gives
one interface over S3, Azure, GCS, local disk and memory, with retries,
multipart, and the standard credential chains (environment, instance
metadata, IRSA, workload and managed identity). Chosen over three SDKs.

## Decision

- **Layout.** `<prefix>/chunks/<h[0..2]>/<sha256>` holds zstd-compressed
  chunks named by the SHA-256 of their *uncompressed* bytes.
  `<prefix>/volume-snapshots/<id>.json` holds one manifest per snapshot.
- **Chunking.** Block images use fixed 4 MiB chunks at aligned offsets; an
  in-place write to a block changes only that chunk. All-zero chunks are not
  stored, which keeps sparse images small. Directories are serialized as a
  tar stream with entries in sorted order and full metadata, then cut with
  FastCDC (min 1, average 4, max 16 MiB), so an inserted file shifts only
  nearby chunk boundaries. 4 MiB matches PBS and keeps request overhead low
  on object stores.
- **Upload.** For each chunk, `HEAD` first; upload only if missing. A
  manifest is written after all of its chunks, so its presence implies
  completeness (requirement 3). Requests run 16 at a time.
- **Download.** Chunks are fetched in parallel, decompressed, and checked
  against their hash; a mismatch or a missing chunk fails the restore.
  Block chunks are written at their offsets into a sparse file; directory
  chunks are fed in order to a tar unpacker.
- **Garbage collection** is mark and sweep: read every manifest (aborting if
  any is unreadable), then delete chunks that none references and that are
  older than a grace period (24 h by default). An upload that finds an
  existing chunk older than half the grace period uploads it again, which
  renews its timestamp, so an upload that finishes within half the grace
  period cannot lose a chunk to a concurrent collection.
- **API.** `snapshot(durable=True)` pushes after the local snapshot and keeps
  nothing if the push fails. `delete(local_only=True)` frees the local copy
  and keeps the remote one. `Volume.from_snapshot(id)` uses the local copy
  if present and otherwise downloads, including snapshots pushed by another
  daemon. `list_volume_snapshots(remote=True)` lists the store.
  `collect_garbage(dry_run)` runs the sweep. The daemon takes
  `[blob_store] url` (or `--blob-store`); credentials come only from the
  environment or the machine's identity.

## Consequences

- Restore downloads the whole snapshot before the volume is usable. For a
  large workspace this is the main cost. **Lazy restore** was the planned
  next step, since built (decision 0016): serve the volume to the VM from the manifest through a
  user-space block device (ublk or NBD) that fetches chunks on first read
  and writes to a local copy-on-write overlay. The manifest already records
  everything that needs (fixed offsets, per-chunk hashes).
- Memory snapshots stayed local at first; ADR 0018 later stored them in
  the same chunk store for hosts of the same CPU model and release.
- Deduplication works across every volume in a store, which also means a
  chunk's existence reveals that some snapshot contains those exact 4 MiB.
  Stores shared between tenants should use separate prefixes or buckets.
  Encryption at rest is the bucket's (SSE-S3, SSE-KMS, Azure SSE); client-
  side encryption would disable cross-snapshot deduplication unless it is
  convergent, and is not implemented.
- Two RustSec "unmaintained" advisories (chrono, humantime) arrive through
  `object_store` and are accepted in `deny.toml` with that reason.
