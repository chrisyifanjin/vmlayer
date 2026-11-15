# Roadmap

- [x] **0. API surface.** Python API, Rust `Backend` trait, design records.
- [x] **A. Working product on Docker.** `vmlayerd`, gRPC contract, Docker
      backend, Python SDK, CLI, end-to-end tests, CI.
- [x] **B. Firecracker backend and guest agent written.** Jailer-based VMs,
      per-VM network namespaces, gRPC agent over vsock (tested end to end in
      a container), block-image volumes, snapshot/restore/fork code paths.
      See decision 0008.
- [x] **1. First boot on a KVM host.** Firecracker 1.18 on aarch64 with
      nested virtualization: boots OCI images, exec, files, pty, egress.
- [x] **2. Snapshot, restore, fork on Firecracker.** Memory snapshots with
      running processes surviving, fork under write load
      (`bench/stress_snapshots.py`). Not yet: uffd working-set prefetch and
      zero-page elision (docs/research.md), bare-metal x86_64 numbers.
- [x] **3. Image pipeline.** The daemon pulls OCI images itself, applies
      layers with whiteouts, builds a cached ext4 per digest (decision 0009);
      `vmlayer pull` pre-builds. Layer sharing via EROFS is the follow-up.
- [ ] **3a. Template snapshots for fast create.** Boot a prepared image once,
      snapshot its ready guest agent, and restore from that snapshot on create.
      This removes the approximately 0.8 s kernel-to-init portion of cold boot
      observed on the nested-KVM development host. Measure on bare metal too.
- [ ] **4. Egress allowlists and quotas** (eBPF or nftables; pids, disk,
      output size).
- [ ] **5. libkrun backend** for macOS development.
- [ ] **6. S3-compatible blob store** for durable snapshots.
- [ ] **7. TypeScript SDK, asyncio client, E2B-compatible facade.**
- [ ] **8. Adapters** for the six runtimes in docs/integrations.md.
- [ ] **8a. Agent Sandbox API compatibility.** Define and test a supported
      subset of its Sandbox lifecycle against the direct-VM host daemon.
      The upstream controller remains Pod-backed; reusing it unchanged means
      a Kata RuntimeClass and one Pod per sandbox.
- [ ] Later: optional multi-host scheduler.
