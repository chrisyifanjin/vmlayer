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
- [x] **4. Egress allowlists and IO limits** (decision 0012): guests cut
      off from the host, each other and cloud metadata; allowlists by
      network or DNS name through a per-VM DNS proxy and nftables sets;
      Firecracker rate limiters for network and disk. Docker refuses both.
- [ ] **5. libkrun backend** for macOS development.
- [x] **6. Blob store for durable snapshots** (S3 and S3-compatible, Azure
      Blob, GCS, shared directory; content-addressed chunks, incremental,
      garbage-collected; decision 0011).
- [ ] **6a. Lazy restore**: serve a volume straight from its manifest
      through ublk or NBD, fetching chunks on first read.
- [ ] **7. TypeScript SDK, asyncio client, E2B-compatible facade.**
- [ ] **8. Adapters** for the six runtimes in docs/integrations.md.
- [ ] **8a. Agent Sandbox API compatibility.** Define and test a supported
      subset of its Sandbox lifecycle against the direct-VM host daemon.
      The upstream controller remains Pod-backed; reusing it unchanged means
      a Kata RuntimeClass and one Pod per sandbox.
- [ ] Later: optional multi-host scheduler.
