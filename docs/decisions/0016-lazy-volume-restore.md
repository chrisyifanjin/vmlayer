# 0016. Lazy restore of block-image volumes through NBD

Status: accepted, 2026-11-17

## Context

ADR 0011 restored a durable snapshot by downloading all of it before the
volume could be attached. Restore time grew with the workspace: seconds
for a small repository, minutes for a large dataset or dependency cache,
even when the agent touches a small part of it. ADR 0011 named lazy restore
as the next step, and the multi-host work (ADR 0014, 0015) makes restores
on a new host common.

## Options considered

**FUSE file.** Serve the image as a file on a FUSE mount and bind-mount it
into the jail. Works without kernel modules, but every guest disk request
crosses FUSE's request path twice, and a FUSE daemon crash leaves a dead
mount behind.

**ublk.** The modern userspace block device (Linux 6.0+, io_uring based)
and the fastest option, but its module is missing from common
distribution kernels (the Ubuntu 24.04 generic kernel on the development
host has no `ublk_drv`).

**vhost-user-blk.** Firecracker can attach a vhost-user block backend, but
its snapshots do not support such devices, which would break memory
snapshots and fork for those sandboxes.

**NBD.** The network block device driver is in every mainstream kernel
(`modprobe nbd`). Configured with ioctls over a Unix socketpair it needs no
userspace tool and no network; the protocol's transmission phase is a few
dozen lines. Firecracker opens `/dev/nbdN` like any disk, so snapshots keep
working. Chosen.

## Decision

- `LazyImage` (blob crate) keeps a sparse local file the size of the
  snapshot and a one-byte-per-chunk marker file. A read touching a missing
  chunk downloads and verifies it first; a partial write to a missing chunk
  downloads it, then writes; a write covering a whole chunk skips the
  download. Chunks the snapshot does not store are zeros and present from
  the start. A background task downloads the rest, 16 at a time, then
  deletes the marker. The marker lets a restarted daemon resume.
- `Volume.from_snapshot` of a block snapshot without a local copy returns
  at once with such a volume when `[blob_store] lazy_restore` is on (the
  default). When the volume is attached to a sandbox, the daemon connects a
  free `/dev/nbdN` to it and serves NBD requests from the image; the
  Firecracker backend creates a device node for it in the jail. The device
  is disconnected when no sandbox holds the volume.
- Snapshotting or cloning a lazy volume first completes the download, so
  everything derived from it is an ordinary file.
- Without the `nbd` module, attaching waits for the download instead.
  Directory volumes (Docker) are still downloaded first.

## Consequences

- Measured on the nested-KVM development host, a 1 GiB volume holding
  300 MB of random data, restored from a local blob store: the restore call
  returns in under 10 ms instead of 0.63 s, and the first file read in the
  booted VM comes at 1.65 s against 1.91 s. With a remote store the eager
  path grows with the volume size; the lazy path does not.
- A guest reading data that is not local waits for that chunk's download;
  reading the whole volume immediately is slower than downloading it first.
- The daemon must stay up while a lazily restored volume is attached and
  incomplete: if it dies, the VM's disk reads fail until it restarts and
  resumes the download (the VM itself may need restarting).
