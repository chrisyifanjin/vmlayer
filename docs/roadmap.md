# Roadmap

- [x] **0. API surface.** Python API, Rust `Backend` trait, design records.
- [x] **A. Working product on Docker.** `vmlayerd`, gRPC contract, Docker
      backend, Python SDK, CLI, end-to-end tests, CI.
- [x] **B. Firecracker backend and guest agent written.** Jailer-based VMs,
      per-VM network namespaces, gRPC agent over vsock (tested end to end in
      a container), block-image volumes, snapshot/restore/fork code paths.
      See decision 0008.
- [ ] **1. First boot on a KVM host.** Run the Firecracker backend for real:
      boot a prebuilt ext4 root, `exec` round-trips, fix what the hardware
      disagrees with. Needs a Linux host with KVM.
- [ ] **2. Snapshot, restore, fork on Firecracker.** uffd memory backend with
      working-set prefetch, zero-page elision, volume clones via reflink.
      Measure restore latency three ways (docs/research.md).
- [ ] **3. Image pipeline.** OCI to ext4/EROFS for microVM backends
      (decision 0008, pending), with layer sharing.
- [ ] **4. Egress allowlists and quotas** (eBPF or nftables; pids, disk,
      output size).
- [ ] **5. libkrun backend** for macOS development.
- [ ] **6. S3-compatible blob store** for durable snapshots.
- [ ] **7. TypeScript SDK, asyncio client, E2B-compatible facade.**
- [ ] **8. Adapters** for the six runtimes in docs/integrations.md.
- [ ] Later: optional multi-host scheduler.
