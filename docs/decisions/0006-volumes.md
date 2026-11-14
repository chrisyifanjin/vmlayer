# 0006. Workspaces are Volumes: separate from the sandbox, invisible by default

Status: accepted, 2026-11-14

## Context

Agent runtimes need somewhere for the agent's work to live: a repo checkout,
installed dependencies, build artifacts. Most of the time that state lives and
dies with one sandbox. But not always:

1. The work outlives the sandbox (multi-day task; sandbox killed by TTL or to
   save money; resume tomorrow).
2. One workspace, several sandboxes in sequence (new image or kernel, same
   files; host died, resume elsewhere).
3. One workspace, several sandboxes in parallel (fork for exploration; run the
   tests in a clean sandbox against the same files).
4. The workspace exists before any sandbox (pre-cloned repo with warm
   dependency cache, used as a template for hundreds of sandboxes).
5. Several workspaces in one sandbox (multi-repo change; a large read-only
   dataset shared by many sandboxes).
6. A human and an agent share the workspace (IDE on a laptop, agent in a
   sandbox). Needs bidirectional sync, not shared block storage. Out of
   scope for v1; the model must not preclude it.

## Evidence

- Fly.io Sprites: one persistent ext4 disk per Sprite, filesystem-only
  checkpoints (~300 ms), restorable any number of times. Memory is kept only
  across warm sleep.
- Modal: sandboxes are ephemeral (5 min default, 24 h max); persistence is
  Volumes mounted at paths plus filesystem snapshots (30-day TTL). Memory
  snapshots expire in 7 days.
- E2B: no separation; `pause` persists the whole root disk plus memory.
  Simple, but every persisted sandbox is a full-disk object bound to a host
  CPU model.
- Firecracker: raw block files only (no qcow2), so copy-on-write must come
  from the host filesystem (reflink on XFS/btrfs) or device-mapper. A VM
  snapshot references its drive files; restoring requires those files as they
  were at snapshot time. Hot-plugging virtio-blk works since v1.16 with
  `--enable-pci`, and the guest must trigger a PCI rescan itself.
- There is no native fork in Firecracker. Fork is snapshot plus N restores;
  memory pages are shared through private mmap of the snapshot file until
  written.

## Decisions

**D1. One primitive, `Volume`. No `Workspace` object.** A volume is a block of
storage with a size, optional name, labels, and snapshots. vmlayer does not
know what a repo or a dependency cache is; a "workspace" is whatever the
platform puts on a volume. Platforms attach their own meaning via `labels`.
Reason: the moment vmlayer understands git, every platform's different idea
of a workspace becomes a feature request against the middle layer.

**D2. Root disk and workspace are separate from day one.** The root disk is
the image plus a thin writable overlay, always disposable. Agent work goes on
a volume. Consequence for roadmap step 1: the vertical slice boots with two
block devices even if the second is a hand-made ext4 file.

**D3. Default is an anonymous workspace volume.** `Sandbox.create` creates a
volume mounted at `workdir` with the same lifetime as the sandbox. Passing a
named `Volume` reuses it; passing `None` runs on the root overlay only (for
callers who want the absolute minimum cold start). Extra volumes, including
read-only shared datasets, go through a separate `mounts` parameter so the
common case has one knob and the rare case has two.

**D4. Single writer, many readers.** A volume is attached read-write to at
most one sandbox at a time. Read-only attachments are unlimited. The daemon
rejects a second read-write attach with an error that says why. Shared
read-write access is a different feature (virtio-fs or a network filesystem,
neither available on Firecracker today) and would arrive as its own
capability, not by loosening this rule.

**D5. Restore and fork clone volumes; they never reattach the original.** A
restored or forked sandbox gets fresh copy-on-write clones of every volume
that was attached when the snapshot was taken. The original named volume is
untouched and stays available to whoever holds it. This is what D4 requires,
and it makes "fork then diverge" the default behaviour with no extra API.

**D6. Two kinds of persistence, named differently.**
- `Sandbox.snapshot()`: memory + root overlay + all attached volumes, taken
  atomically while paused. Resumes mid-execution. Bound to host CPU model and
  kernel, large, intended to be short-lived. This is the fork primitive.
- `Volume.snapshot()`: filesystem only. Cheap, portable across hosts and
  backends, exportable to the blob store, intended to be long-lived. This is
  the template and checkpoint primitive.

**D7. Copy-on-write via reflink, with a fallback.** Volumes and root overlays
are files in the daemon's data directory. Clone is `copy_file_range` with
reflink on XFS or btrfs: metadata-only, milliseconds regardless of size. The
installer probes the data directory's filesystem; on ext4 or others it falls
back to a full copy and logs a warning on every clone. device-mapper thin
provisioning is not used in v1; it buys little over reflink and adds an
operational dependency.

**D8. Snapshots complete locally; durability is opt-in.** `snapshot()` returns
when the reflink is done. `durable=True` additionally waits for upload to the
configured `BlobStore` (ADR 0005). A background policy can upload local
snapshots asynchronously. Garbage collection is by TTL and labels, specified
later.

**D9. Mounts are fixed at creation in v1.** Hot attach needs Firecracker's
PCI mode, which changes the guest device layout and therefore snapshot
compatibility, and needs the guest agent to rescan the bus. The API has
`attach`/`detach` guarded by a `hot_attach` capability that no v1 backend
sets.

**D10. Fixed size, sparse files, ext4 inside.** Every volume has a `size_mb`
with a daemon-configured default. Files are sparse so unused capacity costs
nothing. Online growth (`PATCH /drives` plus `resize2fs` by the guest agent)
is deferred; the API already carries the size so it can be added without a
shape change.

**D11. The daemon owns volumes; backends receive a mount source.** Mirroring
ADR 0003: backends get `Mount { source: VolumeSource, guest_path, read_only }`
where `VolumeSource` is `BlockImage { path }` (Firecracker, libkrun) or
`Dir { path }` (Docker). Creation, cloning, snapshotting and upload all live
in one daemon component. Inside the guest, the agent mounts volumes according
to a mount table the daemon sends at handshake, so images need no `fstab`.

## Consequences

- `Capabilities` gains `volumes` and `hot_attach`.
- `SandboxSpec` gains `mounts`; `SnapshotRef` records the volume snapshots it
  contains so restore can clone them.
- Deployment requirement: put the daemon's data directory on XFS or btrfs for
  cheap clones. Documented in the install guide; the installer checks.
- Case 6 (human/agent sync) stays open. A future `sync` feature would operate
  on a volume from the host side while no sandbox has it read-write.
