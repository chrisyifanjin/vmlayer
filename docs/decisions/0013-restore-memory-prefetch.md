# 0013. Restore: populate guest memory up front over userfaultfd

Status: accepted, 2026-11-17

## Context

Restoring a Firecracker memory snapshot with the `File` memory backend maps
the snapshot's memory file into the new VM. Nothing is read until the guest
touches a page; every first touch is a page fault handled by the host
kernel, and under nested virtualization each fault also costs a VM exit to
the outer hypervisor. Measured on the development host (aarch64, nested
KVM, 512 MiB guest): the restore call takes about 220 ms, but the first
`import requests` afterwards takes 2.8 s against 0.5 s for the second one.
Agents restore to run something heavy right away, so this is the latency
that matters.

## Options considered

**Working-set prefetch** (REAP, ASPLOS 2021; FaaSnap, EuroSys 2022): record
which pages a first restore touches, then on later restores load that set
eagerly and fault the rest lazily. Best memory efficiency; needs a record
phase, a per-snapshot working-set file, and still faults on pages outside
the set.

**Copy everything, eagerly** (Firecracker's own example `fault_all`
userfaultfd handler): on the first fault, or right after restore, copy all
of guest memory in. Simple and complete; costs a private copy of every
non-zero page per restored VM.

**Huge pages** (Firecracker `huge_pages: "2M"`): 512 times fewer faults,
but needs hugetlbfs pages reserved on the host and the userfaultfd backend
anyway for snapshots.

**`MAP_POPULATE` or warming the host page cache**: the file is usually
already cached right after a snapshot; the cost is the faults themselves,
not disk reads.

## Decision

Restore with Firecracker's `Uffd` memory backend. The daemon listens on a
socket in the jail; Firecracker connects during snapshot load and passes
the userfaultfd and the region layout. A handler thread then:

1. walks guest memory in 2 MiB batches, copying each batch from the
   snapshot file with `UFFDIO_COPY`, or mapping it to the shared zero page
   with `UFFDIO_ZEROPAGE` when the batch is all zeros;
2. between batches, serves any fault the guest has taken (a guest vCPU is
   blocked on it), so the guest never waits behind the background copy;
3. when every page is present, closes the userfaultfd and exits.

Zero-page mapping keeps untouched guest memory free: in the measurement
above, 364 of 512 MiB were zero. The handler is bounded per VM and stops
when the sandbox is killed.

Default on (`[firecracker] prefetch_memory = true`).

## Consequences

- Measured: the first heavy process after a restore drops from 2.8 s to
  0.75 s, the first trivial command from 110 to 35 ms, and full population
  of a 512 MiB guest takes 160 to 220 ms. An episode reset in the framework
  benchmark drops from 3.1 s to 1.0 s.
- Each restored VM holds a private copy of the snapshot's non-zero pages
  (148 MiB in the example) instead of sharing them through the page cache
  until written. Fork-heavy deployments that care more about memory than
  first-use latency can set `prefetch_memory = false`.
- If the daemon dies during the first fraction of a second after a
  restore, before population finishes, the guest blocks on its next missing
  page. The window is short; the VM is then killed by the TTL or by the
  operator.
- Working-set recording (REAP) remains a possible refinement for very large
  guests, where copying all non-zero memory would take longer than the
  workload needs.
