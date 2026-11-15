# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/). Until 1.0 the API may change between minor
versions; every change to the SDK surface is listed.

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
