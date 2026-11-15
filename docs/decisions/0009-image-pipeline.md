# 0009. Image pipeline: pull OCI layers, unpack, build one ext4 per image

Status: accepted, 2026-11-15

## Context

MicroVM backends boot a block device, not a container image. Users want to
write `Sandbox.create("python:3.12")` for every backend (ADR 0003, 0006).
Something has to turn an OCI image into an ext4 root the Firecracker backend
clones per sandbox. DSec (docs/research.md) shows the eventual answer is
layered EROFS with lazy loading; that is a later optimisation, not the
first implementation.

## Decisions

**D1. The daemon pulls from registries itself with `oci-client`**, not via
Docker. Production Firecracker hosts should not need a Docker engine. Auth
comes from `~/.docker/config.json` (`auths`) so `docker login` keeps
working; anonymous otherwise. The platform is the host's (`linux/<arch>`).

**D2. Layers are applied to a scratch directory with OCI whiteout
semantics** (`.wh.<name>` deletes, `.wh..wh..opq` empties a directory),
in order, preserving permissions, symlinks and hard links. Ownership is
preserved when the daemon runs as root (which Firecracker needs anyway).

**D3. One ext4 image per image digest, built with `mkfs.ext4 -d`** so no
mount and no loop device is needed. Size is the unpacked size plus 30% and
at least 256 MiB, as a sparse file. The image is stored under
`<data_dir>/images/<sha256>/rootfs.ext4` beside `config.json` (the OCI
config: entrypoint, cmd, env, workdir) and is never written to again: the
backend clones it copy-on-write per sandbox. A `by-ref` directory maps
`python:3.12` to the digest it resolved to, refreshed when the tag is pulled
again.

**D4. Resolution is a single `ImageResolver` for microVM backends:** an
absolute path to an existing `.ext4` is used as-is (for hand-built images
and tests); anything else is treated as an OCI reference. The Docker backend
keeps resolving through the engine.

**D5. Build steps are observable.** Pull, unpack and mkfs log at info with
the digest and sizes; a failed build leaves nothing behind (scratch dir and
partial image are removed).

## Consequences

- Host requirement for microVM backends: `e2fsprogs` (`mkfs.ext4` ≥ 1.43
  for `-d`). The daemon reports its absence precisely.
- First boot of an image costs a pull plus a mkfs (seconds to a minute for
  large images); subsequent sandboxes clone the cached ext4 in
  milliseconds. Pre-pulling is `vmlayer image pull` (CLI, later).
- Layered EROFS, shared read-only base images via virtio-pmem and lazy
  pulls remain the plan for density (docs/research.md); this record will be
  superseded then, with the `Rootfs::Ext4` path kept for compatibility.
