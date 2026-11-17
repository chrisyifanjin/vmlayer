# Design decisions

Every non-obvious choice gets a short record here before it is implemented:
what was decided, what was researched, what it rules out. Numbered, never
edited after acceptance; a reversal is a new record that supersedes the old one.

Format: context, options considered (with evidence), decision, consequences.

| # | Decision | Status |
|---|---|---|
| [0001](0001-rust-for-core.md) | Rust for the core, daemon and guest agent | accepted |
| [0002](0002-daemon-plus-sdks.md) | Daemon plus thin SDKs, not an in-process library | accepted |
| [0003](0003-backend-trait-boundary.md) | Backends consume `Rootfs`, never OCI references | accepted |
| [0004](0004-apache-2-license.md) | Apache-2.0 | accepted |
| [0005](0005-cloud-agnostic.md) | Cloud-agnostic: Linux + KVM only, S3-compatible blob store, no managed services | accepted |
| [0006](0006-volumes.md) | Workspaces are Volumes: separate from the sandbox, anonymous by default, single writer, cloned on restore/fork | accepted |
| [0007](0007-daemon-stack.md) | Daemon stack: tonic gRPC over unix socket, SQLite, bollard for Docker | accepted |
| [0008](0008-firecracker-backend-design.md) | Firecracker: jailed VMs with fixed drive names, one netns each, gRPC guest agent over vsock, fork = snapshot + restores | accepted |
| [0009](0009-image-pipeline.md) | Image pipeline: pull OCI layers in the daemon, unpack with whiteouts, one cached ext4 per digest via `mkfs.ext4 -d` | accepted |
| [0017](0017-e2b-wire-compatibility.md) | E2B wire compatibility: a gateway serving E2B's REST control plane and envd Connect services on vmlayer, using the e2b package's generated server code | accepted |
| [0016](0016-lazy-volume-restore.md) | Lazy restore of block volumes: a sparse local image filled on demand from the blob store, served to Firecracker as `/dev/nbdN` | accepted |
| [0015](0015-rescheduling.md) | Rescheduling: periodic durable checkpoints of the workspace and a workload record; any daemon with rescue on recreates a sandbox whose lease expired | accepted |
| [0014](0014-hosts-placement-and-leases.md) | Several hosts: heartbeats and leases in the blob store via conditional writes, placement in the SDK, single-instance sandboxes with fencing | accepted |
| [0013](0013-restore-memory-prefetch.md) | Restore memory snapshots over userfaultfd and copy all non-zero guest memory into place at once, zero-mapping the rest | accepted |
| [0012](0012-network-isolation-egress-and-io-limits.md) | Guests cannot reach the host, other VMs or cloud metadata; egress allowlists via a per-VM DNS proxy filling nftables sets; Firecracker rate limiters for IO | accepted |
| [0011](0011-durable-snapshots-in-object-storage.md) | Durable volume snapshots as content-addressed chunks in object storage (S3, Azure Blob, GCS, directory) via `object_store`; manifest last; mark-and-sweep GC | accepted |
| [0010](0010-python-stub-compatibility.md) | Python stubs generated with an old pinned toolchain and the protobuf version pin stripped, so the SDK coexists with frameworks on protobuf 5, 6 or 7 | accepted |
