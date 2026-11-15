# 0008. Firecracker backend: jailed VMs with fixed drive names, one netns each, gRPC guest agent over vsock

Status: accepted, 2026-11-15

## Context

The Firecracker backend has to deliver what the Docker backend cannot:
memory snapshots, sub-second restore and `fork`. Three things decide how
hard that is: where disks live (snapshots record drive paths), how guests
get network identity (a restored guest keeps its old one), and how the host
talks to the guest (every backend needs exec, files, pty).

## Decisions

**D1. The jailer, one chroot per VM, fixed file names inside.** Firecracker
snapshots record `path_on_host` for every drive and will not load if the
paths are missing. With the jailer each VM has its own chroot, so the
*same* path (`/root.ext4`, `/vol0.ext4`, `/vmlinux`) names a different file
in each jail. Restore populates a fresh jail with clones and loads the
snapshot unchanged: no `PATCH /drives`, no path rewriting, and the jailer's
own isolation (chroot, uid/gid drop, seccomp) comes for free. Disks are
reflink clones (root) or hard links (volumes) so the jail costs nothing.

**D2. One network namespace per VM, identical guest-side addressing.**
Every guest is `172.16.0.2/30` with gateway `.1` and the same MAC, behind
`tap0` inside `netns vml-N`. The namespace reaches the host over a veth pair
on a per-VM /30 from a `10.200.0.0/16` pool; the host masquerades egress and
the namespace DNATs its veth address to the guest. Because a restored or
forked VM gets a *new* namespace but the *same* guest config, snapshots need
no network rewriting and N forks of one parent never collide. `expose` is a
loopback TCP proxy to the namespace's veth address. This is the layout E2B
and firecracker-containerd converged on; a flat bridge would need per-guest
IP/MAC rewriting inside the restored guest.

**D3. The guest agent speaks gRPC over vsock and serves the same `Processes`
and `Files` services the daemon does.** A backend handle is then a thin gRPC
client, and the process table, output buffering and re-attach semantics are
one shared crate (`procs/`) used on both sides. vsock reaches the host as a
unix socket with a one-line `CONNECT <port>` handshake, which a tonic
connector performs. The agent also serves a small `Guest` service: mount,
entrypoint, wait-for-port, fsfreeze, PCI rescan, shutdown. For testing it can
listen on TCP, so the whole agent is exercised in a container in CI.

**D4. The agent is PID 1 and forks a reaper.** Tokio's child handling and a
`waitpid(-1)` reaper cannot share a process, so PID 1 stays a tiny reaper
that forwards SIGTERM, and the agent runs as its child. PID 1 duties (pseudo
filesystems, loopback, hostname, DNS from the kernel command line) happen
before the fork. The agent is injected into each root image clone with
`debugfs`, so any ext4 root image works without rebuilding it.

**D5. `fork` is snapshot plus N restores, orchestrated by the daemon.**
Firecracker has no fork. The daemon takes one snapshot and leaves the parent
paused, clones the parent's volumes for every child *while it is paused* (so
the disks match the memory image exactly), resumes the parent, and restores
the snapshot once per child. Each child is a fresh jail with cloned disks
and the memory file mapped privately, so pages are shared copy-on-write
until written. There is no `fork` in the backend trait. Guests pause for the snapshot (tens of
milliseconds) and the agent `fsfreeze`s volumes first so the disk clones are
consistent. `pci=off` keeps the device layout stable across snapshots; hot
attach would need PCI mode and is left off the v1 capability list.

**D6. Volumes have a representation chosen by the backend.** Containers mount
directories; microVMs attach block devices. `Backend::volume_source_kind`
tells the daemon which to create (`mkfs.ext4` on a sparse file for block
images), and a volume keeps its kind for life: attaching a directory volume
to a microVM is refused with an explanatory error rather than converted.

**D7. vsock connections do not survive a snapshot, on either side.**
(Added after the first runs on real KVM.) Firecracker resets the guest's
vsock transport when it *takes* a snapshot, not only when it loads one, so
the parent's agent connection dies at every snapshot and fork. And a guest
resuming from a snapshot processes its pending transport-reset shortly
*after* it starts running: a host connection made in that window is dropped
by the guest without any notice to the host, and calls on it hang forever.
Three rules follow:
- The agent connection is a replaceable `Link` shared by the sandbox handle
  and every process handle. After each snapshot and resume the handle
  installs a fresh connection.
- A connection is only handed out after it has answered `Ready` twice, a
  moment apart, and every channel runs HTTP/2 keepalive so a silently dead
  one becomes an error within seconds.
- Process handles outlive resets: the output stream reopens on the new
  connection and skips the history the agent replays up to what was already
  delivered; `wait` retries. Stdin and pty streams do not survive a snapshot.

**D8. Sandbox-wide environment and working directory are agent state.** A
container gets these from its engine; a microVM has no such thing, so the
backend sends them once with `Guest.Configure` and the agent applies them to
every process (a request's own values win).

**D9. Restore attaches volumes in the snapshot's drive order.** The guest's
block devices are fixed by the snapshot (`vdb` is whatever was the first
volume), so restored volumes are matched to drives by guest path, not by the
order the caller lists them.

## Consequences

- Host requirements: Linux with `/dev/kvm`, `firecracker` + `jailer`,
  an uncompressed guest kernel, `ip`, `nft`, `e2fsprogs` (`mkfs.ext4`,
  `debugfs`), and root (taps, namespaces, chroot). The daemon reports
  precisely which is missing in `vmlayer info`.
- Verified on real KVM (Firecracker 1.18, guest kernel 6.1, nested
  virtualization on Apple Silicon via `scripts/kvm-vm/`): boot from an OCI
  image, exec, files, pty, networking, block-image volumes, memory
  snapshots, restore and fork, including `bench/stress_snapshots.py` with a
  process writing to the volume throughout. Bare-metal x86_64 has not been
  run yet.
- The OCI image pipeline (roadmap step 3) will produce the ext4 root images
  this backend consumes; until then the image reference is a path to one.
