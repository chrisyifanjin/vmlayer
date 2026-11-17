# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/). Until 1.0 the API may change between minor
versions; every change to the SDK surface is listed.

## [Unreleased]

### Added
- Image pipeline for microVM backends: the daemon pulls OCI images itself
  (auth from `~/.docker/config.json`), applies layers with whiteout
  semantics, and builds one cached ext4 per digest with `mkfs.ext4 -d`.
  `Sandbox.create("python:3.12", backend=FIRECRACKER)` no longer needs a
  hand-built image.
- `PullImage` RPC, `vmlayer pull`, `Client.pull_image` / `client.pullImage`
  to resolve an image ahead of time on any backend.
- Framework adapters in `vmlayer.integrations`: a mini-swe-agent
  `Environment` and an AutoGen `CodeExecutor`, as extras
  (`vmlayer[minisweagent]`, `vmlayer[autogen]`).
- Docker sandboxes get a process limit (`[docker] pids_limit`, default 4096)
  so a fork bomb stays inside its sandbox.
- `Backend::cleanup_dir`: volumes written as root inside containers are
  removed through the engine when the daemon cannot delete them itself.
- Firecracker backend verified on real KVM (aarch64, nested): boot from an
  OCI image, exec, files, pty, block-image volumes, memory snapshots,
  restore and fork. `scripts/kvm-vm/` sets up a KVM host on a laptop.
- Firecracker boot templates (`[firecracker] template_snapshots`): after the
  first cold create of an image and VM shape, later creates restore a new
  microVM from a data-free on-disk snapshot.
- Firecracker boot admission control (`[firecracker] max_concurrent_boots`,
  default one per host CPU) and a configurable `boot_timeout_secs`: a burst
  of creates queues instead of timing out.
- Framework bridges for Deep Agents (`vmlayer[deepagents]`) and OpenHands
  (`vmlayer[openhands]`, experimental).
- Benchmarks with recorded samples in `docs/benchmarks.md`, including each
  agent framework's own sandbox against the same framework on vmlayer.
- Durable volume snapshots in object storage (decision 0011). Configure
  `[blob_store] url` (`s3://`, `az://`, `gs://`, `file://`) and call
  `Volume.snapshot(durable=True)`: the snapshot is stored as
  content-addressed 4 MiB chunks, so later snapshots upload only changed
  blocks. `VolumeSnapshot.delete(local_only=True)` frees the local copy,
  `Volume.from_snapshot` downloads snapshots pushed by any daemon sharing the
  store, `Client.list_volume_snapshots(remote=True)` lists them, and
  `Client.collect_garbage()` removes unreferenced chunks. Same in the
  TypeScript SDK and the CLI (`vmlayer volume snapshot --durable`,
  `ls-snapshots --remote`, `rm-snapshot --local-only`, `gc`). Tested against
  MinIO (S3) and Azurite (Azure Blob) in CI.
- Firecracker end-to-end tests (`tests/test_firecracker.py`) and a CI job
  that runs them, the snapshot stress test and the benchmark on a GitHub
  x86_64 runner with KVM.

- Egress allowlists and IO limits on the Firecracker backend (decision
  0012). `Sandbox.create(egress=["pypi.org", "*.pythonhosted.org:443",
  "10.0.0.0/8:5432"])` rejects every other destination; a per-VM DNS proxy
  answers only allowed names and opens their addresses in nftables sets for
  the record's TTL, whatever resolver the guest asks.
  `limits=IoLimits(network_mbps=..., disk_mbps=..., disk_iops=...)` uses
  Firecracker's rate limiters. New capabilities `egress_policy` and
  `io_limits`; the Docker backend refuses both with `BackendUnsupported`.
- Several hosts without a scheduler service (decision 0014). Daemons with
  `[cluster] advertise` publish heartbeats in the blob store;
  `System.ListHosts` / `Client.list_hosts()` list them; `vmlayer.Cluster`
  places sandboxes by backend, labels and free memory and finds them again.
  `Sandbox.create(lease="name")` keeps at most one live sandbox per name
  across hosts, using the store's conditional writes, and a daemon that
  loses or cannot renew a lease kills its sandbox.
- `vmlayer.e2b`: an E2B-shaped Python facade (`Sandbox.create / connect /
  list`, `commands.run` with background handles, `files.*`, `get_host`,
  E2B's exception types) so code written for the E2B SDK runs after changing
  one import. Templates are labelled vmlayer snapshots (`create_template`).
- `Sandbox.created_at` in the Python SDK.
- Firecracker restores populate guest memory up front over userfaultfd
  (decision 0013, `[firecracker] prefetch_memory`, default on): all-zero
  ranges map to the shared zero page, the rest is copied in 2 MiB batches
  while guest faults are served first. On the nested-KVM host the first
  heavy process after a restore drops from 2.8 s to 0.75 s.
- Firecracker boot templates are pruned to the most recently used
  `template_limit` (default 4), at startup and after each build.

### Fixed
- Security, Firecracker: guests could reach the cloud instance metadata
  service (and with it the host's cloud credentials), any service on the
  host through their veth link, and other guests. Metadata endpoints are
  now always dropped, the host drops new connections from guests except DNS
  to their proxy, and guest-to-guest forwarding is dropped.
- Firecracker: a file written to a volume just before the sandbox was
  killed could be missing from the volume, because the guest's page cache
  died with the VM. The backend now freezes (flushes) the guest's volumes
  before killing it, resuming a paused guest first.
- Firecracker: microVMs had no network egress on hosts that also run Docker,
  whose FORWARD policy drops routed traffic. The backend now accepts the
  sandbox address pool in Docker's `DOCKER-USER` chain when it exists.

### Changed
- Python SDK now requires `protobuf>=5.26` and `grpcio>=1.68` (was
  effectively protobuf 7.35+) and imports under protobuf 5, 6 and 7, so it
  installs next to frameworks that pin protobuf (decision 0010).

## [0.1.0] - 2026-11-15

First release: a working sandbox runtime on Docker, and the complete (but
not yet hardware-tested) Firecracker backend.

### Added
- `vmlayerd` daemon: gRPC `vmlayer.v1` API over a unix socket or TCP with a
  bearer token, SQLite state, TTLs, process output buffering and re-attach,
  reattach to running sandboxes after a restart.
- Docker backend: exec, spawn with stdin, pty, files, pause/resume,
  filesystem snapshots and restore, port exposure, entrypoints,
  wait-for-port; engine discovery for Docker Desktop, colima, OrbStack,
  Rancher; bind-mount visibility probe.
- Volumes: named and anonymous, single-writer rule, copy-on-write snapshots
  and clones, directory and ext4 block-image representations.
- Python SDK (`sdk/python`) and TypeScript SDK (`sdk/typescript`) with the
  same surface; `vmlayer` CLI.
- Firecracker backend and guest agent (jailer, per-VM network namespaces,
  gRPC over vsock, memory snapshots, fork). Unit-tested and the agent tested
  in a container; first boot on a KVM host pending.
- Design records 0001–0008, research notes, integration study of six agent
  runtimes.
