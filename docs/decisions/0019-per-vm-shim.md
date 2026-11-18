# 0019. A shim process per microVM; the daemon leaves the data path

Status: accepted, 2026-11-18. Supersedes the process model implied by
decisions 0012 (DNS proxy), 0013 (memory fill), 0014 (fencing) and 0016
(lazy volumes), all of which ran inside the daemon.

## Context

The first external review (A1) pointed out a contradiction. The design
says microVMs survive a daemon stop or crash, and they did: Firecracker
kept running. But the daemon served the VM's lazily restored disk over NBD,
filled its restored memory, ran its egress DNS proxy and its expose
proxies, and renewed the lease that fenced it. When the daemon went away
the VM kept running without disk reads for missing blocks, without DNS,
without its exposed ports, and without fencing.

## Options considered

**Keep everything in the daemon and make it crash-proof.** Restarting
quickly does not help a guest blocked on a disk read, and fencing in the
daemon cannot work when the daemon is the thing that died.

**Move the services into the guest.** The guest is untrusted; it must not
hold blob-store credentials or enforce its own egress policy.

**A per-VM helper process, like containerd's shim v2** (also Kata's shim
and firecracker-containerd's per-VM agent): a small process per VM that
owns the VM's runtime state, outlives the daemon, and dies with the VM.
Chosen.

## Decision

`vmlayer-shim` (installed next to `vmlayerd`) is started by the backend for
every VM, in its own session, from a JSON spec written next to the jail:

- it starts the jailer, so Firecracker is its child and its exit is known;
- it serves lazy volumes (creates the jail's device node, connects
  `/dev/nbdN`, downloads the rest in the background), restore memory over
  userfaultfd, the egress DNS proxy, and expose proxies;
- it holds the VM's fencing deadline and kills the VM when it passes;
- when Firecracker exits it records how (`shim.exit`), disconnects the
  devices, closes the lazy images cleanly and exits.

The daemon gives orders over a Unix socket next to the jail (one JSON
request per connection: status, extend deadline, expose, unexpose, stop)
and re-attaches to the shim after a restart.

A lazy image has exactly one writer. Before handing a volume to a sandbox
the daemon closes its own copy (waiting for in-flight writes); after the
sandbox is gone it resumes the download from the chunk markers the shim
persisted. A daemon restarting while a sandbox runs leaves attached
volumes alone. Read-only attachments download first.

Fencing (with ADR 0014's host lease): after every host-lease renewal the
daemon pushes the lease's expiry to the shims of its single-instance
sandboxes as their deadline. If renewals stop, each shim kills its VM at
the deadline, which is no later than the moment another host may take the
name over.

## Consequences

- Tested on KVM: with the daemon killed, a VM read a lazily restored file
  correctly, resolved DNS through its egress policy, and served its exposed
  port; the restarted daemon re-attached. A single-instance VM was killed
  by its shim when the host lease ran out with no daemon, and its name was
  free again.
- One extra small process per VM (about 16 MB on disk, a few MB of memory).
- The Docker backend has no shim: Docker containers do not need one for
  disk, DNS or proxies, but they cannot be fenced without the daemon; the
  daemon fences them itself while it runs (capability `fencing` is false).
