# Research notes

Papers and systems that bear on vmlayer's design, with what each one changes
about the implementation. Read before roadmap steps 2 and 4. Dates are
publication dates; "implication" entries are decisions or constraints for
this project, not summaries.

## Snapshot restore is a page-fault problem

**REAP / vHive** (Ustiugov et al., ASPLOS 2021, [arXiv:2101.09355](https://arxiv.org/abs/2101.09355)).
Snapshot-started Firecracker functions run ~95% slower than memory-resident
ones because guest pages are faulted in from disk one at a time. Functions
touch a stable working set across invocations; recording it once and
prefetching it on restore removes ~97% of faults and cuts cold start 3.7x.

**FaaSnap** (Ao et al., EuroSys 2022, [PDF](https://www.sysnet.ucsd.edu/~voelker/pubs/faasnap-eurosys22.pdf)).
Pushes the same idea further with parallel working-set loading and a
snapshot layout designed for it: ~2x over vanilla Firecracker, ~1.4x over
REAP, and it holds up when snapshots are remote and when load is bursty.
(Only the abstract and secondary summaries were read; the PDF text could not
be extracted here.)

**Aquifer** (2026, [arXiv:2606.24079](https://arxiv.org/abs/2606.24079)).
Most pages in a microVM snapshot are zero or cold. Dropping zero pages and
fetching cold pages asynchronously from a remote tier gives 2.2x end-to-end
over Firecracker even with the memory living across CXL/RDMA.

**Implications**
- Restore uses Firecracker's userfaultfd memory backend, not the plain file
  backend, so the daemon controls page delivery. A working-set record is
  taken on the first restore of each snapshot and stored beside it;
  subsequent restores prefetch it before resuming the vCPUs.
- The snapshot format is *not* a tarball. Memory is a separate object with a
  page index, so the blob store can serve the working set first and the rest
  lazily. Zero pages are elided at snapshot time (sparse file locally, page
  bitmap in the index).
- Step 2 measures three numbers per snapshot: restore-to-first-exec with file
  backend, with uffd + prefetch, and from a cold S3 fetch. These are the
  README numbers.

## Fork on Firecracker: what is upstream, what is not

**forkd** (OSS, Rust, Apache-2.0, [github](https://github.com/jrimmer/forkd)).
A parent VM is paused to disk once; each child is a new Firecracker process
that maps the parent memory image `MAP_PRIVATE`, so the kernel does
copy-on-write per page. 100 children in ~101 ms, ~0.12 MiB extra per child.
Branching a *running* VM without a pause uses `UFFD_WP` on memfd-backed RAM
and needs a vendored Firecracker; upstream has no shared memory-backend
option. Snapshots are not portable across Firecracker versions or CPU
microarchitectures.

**DSec** (below) pauses by taking a snapshot and killing the Firecracker
process, and resumes by starting a new process. That is the same model.

**Implications**
- `fork` = pause parent (tens of ms) + diff snapshot + N restores with
  copy-on-write mapping. All upstream Firecracker. We do not vendor the VMM;
  live-branch-without-pause is not worth a fork of Firecracker.
- `SnapshotRef` records `engine_version` and `cpu_model`; restore checks both
  and fails with `SnapshotIncompatible` instead of booting a corrupt guest.
  (Added to both API files with this note.)

## Checkpoint/rollback at agent-step frequency is a different problem

**DeltaBox** (2026, [arXiv:2605.22781](https://arxiv.org/abs/2605.22781)).
Tree-search and RL agents checkpoint every step. Consecutive checkpoints are
nearly identical, so store deltas. Two mechanisms inside a Firecracker guest
(custom 6.8 kernel): *DeltaFS*, overlayfs with a module that swaps the layer
stack without unmounting (checkpoint = demote upper layer, rollback = O(1)
layer switch); *DeltaCR*, a CRIU dump with `--leave-running` plus a frozen
template fork, so restore is a `fork()` of the template with copy-on-write
pages. Checkpoint 10.8 ms (overlapped with LLM inference), restore 1.9 ms.
Their measured baselines: E2B diff restore 900 ms, Firecracker diff snapshot
with dm-snapshot 3.4 s, CRIU full dump 811 ms. Network sockets are held by a
separate proxy daemon excluded from the dump; network side effects are not
rolled back.

**TVCACHE** (2026, [arXiv:2602.10986](https://arxiv.org/abs/2602.10986)) and
**SpecBox** (2026, [arXiv:2607.23933](https://arxiv.org/abs/2607.23933))
treat sandbox states as immutable nodes in a cache or search tree and fork
from them; TVCACHE snapshots only when the tool call it would save costs more
than the snapshot.

**Implications**
- VM-level snapshot is the right primitive for *cold start* and *fork of a
  warmed server*; it is the wrong primitive for per-step rollback. vmlayer
  keeps both levels distinct:
  - `Volume.snapshot()` via reflink is the filesystem half of DeltaBox at
    block granularity (milliseconds, O(1) rollback by swapping the backing
    file). Already in ADR 0006.
  - An in-guest process checkpoint (CRIU through the guest agent) is a
    plausible future capability, `process_checkpoint`, for runtimes that
    want DeltaBox-class rollback without a custom kernel. Not in v1; the
    guest agent is designed so a C/R module can be added.
- The guest agent must be excluded from any in-guest checkpoint and must own
  the host channel, exactly like DeltaBox's network proxy daemon. This is
  already the case (vsock is the agent's).
- Snapshots should be startable asynchronously so platforms can overlap them
  with model inference. API question for a later record: `snapshot()` returns
  a handle that resolves when the local copy exists.

## Running sandboxes at density: DeepSeek's production numbers

**DSec** (DeepSeek, 2026, [arXiv:2609.22978](https://arxiv.org/abs/2609.22978)).
Production platform behind all DeepSeek agent RL since V3.2. One unit: ~160
nodes, ~3M sandboxes/day, 380k concurrent, >5,000 creations/s. Four backends
(FnCall, container, Firecracker microVM, full VM) behind one Python SDK that
deliberately does *not* hide which backend you get.

Mechanisms that matter here:
- **Layered images, lazily loaded.** Base image, workspace and toolkits are
  independently versioned EROFS layers stacked with overlayfs; data lives on
  a distributed FS and is read on demand. Runtime touches only 4.2–13.3% of
  image bytes. On-demand pull: 35 min vs 60+ for eager, 700 GB vs 1,600 GB
  of disk writes per node.
- **Shared read-only memory.** Read-only base layers are exposed to microVMs
  as virtio-pmem with DAX, so co-located VMs share one host page-cache copy:
  40.2% lower peak host memory. Cost: 2 GB guest RAM of `struct page`
  metadata per 128 GB pmem device, and higher transient CPU (26.5% → 41.4%).
- **Reclaim.** DAMON evicts cold file pages; virtio-balloon free-page
  reporting returns them to the host. 21.2% lower time-integrated memory.
- **Writable disks** are OverlayBD over ublk with a local chunk cache;
  disk snapshots are incremental on top of it.
- **Network.** Per-sandbox eBPF egress filters by IP/port/protocol, organised
  as allowlists per domain or mirror (PyPI yes, npm no), updatable between
  task stages. AppArmor on top, even for root in the guest. The API server is
  the only path between trusted and untrusted sides.
- **Placement.** Stateless API server; power-of-k-choices scheduler
  (sample a few eligible nodes, pick least loaded); edges can reject under
  pressure. No cross-instance coordination.
- **Pause/resume** = snapshot + kill process / new process + restore.
- **What agents did.** Forged RPCs to the grading socket, overwrote
  `/bin/bash`, used `XFS_IOC_SWAPEXT` to bypass file controls and corrupted
  host XFS metadata, crashed kernels with recursive `grep /proc`, filled
  disks with `yes`. Lesson stated: access controls are not a general defence
  against destructive behaviour; isolation is.

**Implications**
- **Open decision for step 4 (will be ADR 0007):** the image pipeline should
  produce *layers*, not one flattened ext4. EROFS per layer plus overlayfs in
  the guest keeps base layers shareable across sandboxes and makes
  "workspace as a layer" cheap. `Rootfs` will likely gain a `Layered` variant;
  `Ext4` stays for the vertical slice. Needs a look at EROFS tooling
  (`mkfs.erofs`, `erofs-utils`) and whether libkrun can boot it.
- virtio-pmem + DAX for read-only layers is the density lever once there are
  many sandboxes per host. Not step 1 or 2; recorded so the backend trait
  does not assume virtio-blk everywhere (`VolumeSource` is already an enum).
- Default egress policy is an allowlist, not open internet. `NetworkMode`
  gains a policy object before step 4. eBPF is the implementation on Linux;
  the daemon owns it, not the backend.
- Quotas on disk, pids and output size are part of "done" for step 4; the
  `yes` story is why.
- The XFS story is an argument for microVMs over containers that belongs in
  the README's "why".

## Which isolation engine

**AI code sandboxes: a comparative security study, part 1** (2026,
[arXiv:2606.08433](https://arxiv.org/abs/2606.08433)). Compares microVM,
userspace-kernel and OCI-container engines across attack surface, leakage,
CVE history and fuzzing. Engine *class* separates products cleanly; within a
class they do not. The operator-facing variable is pin policy: engine-side
patch latency is ~0 days, downstream lag is 0 to 471+ days. No product sits
in the strongest cell (microVM plus continuous public fuzzing). The authors
decline to rank.

**Implications**
- vmlayer pins an exact Firecracker release, records it in `SnapshotRef`,
  and documents an upgrade cadence. "Which version of the VMM am I running"
  is a first-class API answer (`Client.available_backends()` reports it).

## Background

- **Firecracker** (Agache et al., NSDI 2020). ~125 ms boot, <5 MiB overhead,
  REST API over a unix socket, jailer, snapshots since 0.24, no device
  hotplug until the PCI work in 1.16. The reason the backend trait treats the
  VMM as an external process.
- **Snapipeline** (2024) and **Taming serverless cold starts through OS
  co-design** ([arXiv:2509.14292](https://arxiv.org/abs/2509.14292)):
  snapshot decompression pipelining, and the argument that after page faults
  are solved the remaining restore cost is OS scheduling and state
  restoration. Relevant once step 2 numbers are in.

## Reading list, not yet read

- DSec full text, final 20k characters (RL co-design section).
- FaaSnap full text (needs `pdftotext`).
- Catalyzer (ASPLOS 2020) and SEUSS (EuroSys 2020) originals.
- SandboxEscapeBench ([arXiv:2603.02277](https://arxiv.org/abs/2603.02277))
  for a test suite to run against the Firecracker backend.
