# vmlayer

[![CI](https://github.com/chrisyifanjin/vmlayer/actions/workflows/ci.yml/badge.svg)](https://github.com/chrisyifanjin/vmlayer/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**The layer between agent runtimes and microVMs.**

One sandbox API for running untrusted agent code: Firecracker in production,
libkrun on your Mac, Docker in CI. Snapshot, restore and fork as first-class
operations. Not a hosted product: a daemon plus SDKs you embed in your own
platform.

```python
from vmlayer import Sandbox, Volume

with Sandbox.create("python:3.12", memory_mb=512) as sb:
    sb.files.write("/work/main.py", "print('hello from a sandbox')")
    print(sb.exec("python /work/main.py").stdout)

    snap = sb.snapshot()        # whole sandbox, restorable later
    sb2 = Sandbox.restore(snap) # own clone of every volume

# a workspace that outlives any one sandbox
repo = Volume.create(name="proj-42", size_mb=8192)
with Sandbox.create("python:3.12", workspace=repo) as sb:
    sb.exec("git clone https://example.com/repo .")
ckpt = repo.snapshot()                          # files only, milliseconds
with Sandbox.create("node:22", workspace=repo) as sb:
    sb.exec("npm test")                         # same files, different image
```

## Status

| Piece | State |
|---|---|
| `vmlayerd` daemon, gRPC API, Python SDK, TypeScript SDK, `vmlayer` CLI | working, tested end to end |
| Docker backend (Linux, macOS, CI) | working: exec, pty, files, volumes, filesystem snapshots, restore, expose, TTLs |
| Volumes: named/anonymous, single-writer rule, copy-on-write snapshots and clones | working (directory and ext4 block-image kinds) |
| Guest agent (PID 1 in the VM; exec, files, pty, mounts over vsock) | working, tested end to end in a container |
| Image pipeline for microVMs (OCI pull → cached ext4, no Docker needed) | working, tested on Linux with a real pull |
| Firecracker backend (memory snapshots, fork) | working; end-to-end tests in CI on x86_64 KVM and locally on aarch64 nested KVM ([setup](docs/firecracker.md), [benchmark](docs/benchmarks.md)) |
| libkrun backend (macOS microVMs) | planned |
| Durable volume snapshots in S3, Azure Blob, GCS or a shared directory (incremental, deduplicated, move between hosts) | working; tested against MinIO and Azurite in CI ([usage](docs/usage.md#durable-snapshots-and-moving-workspaces-between-hosts), [design](docs/decisions/0011-durable-snapshots-in-object-storage.md)) |
| Framework adapters: mini-swe-agent `Environment`, AutoGen `CodeExecutor` | working, tested against the real frameworks ([docs](docs/integrations.md)) |
| Deep Agents `BaseSandbox`, OpenHands `RemoteWorkspace` | working in local Firecracker experiments ([docs](docs/integrations.md)) |
| E2B-compatible facade, asyncio client, more adapters (ADK, SWE-ReX, smolagents) | planned |

The API is designed for all backends; what a backend cannot do it refuses
with `BackendUnsupported`, never approximates. `Sandbox.capabilities` says
which is which.

## Quick start

Requirements: Docker (Docker Desktop, colima, OrbStack or a Linux engine),
Rust 1.80+, `protoc`, Python 3.10+ with [uv](https://docs.astral.sh/uv/).

```bash
git clone https://github.com/chrisyifanjin/vmlayer && cd vmlayer
cargo build --release                      # vmlayerd and vmlayer in target/release
./target/release/vmlayerd &                # listens on ~/.vmlayer/vmlayerd.sock
./target/release/vmlayer info
./target/release/vmlayer run python:3.12-alpine 'python3 -c "print(6*7)"'
```

Python SDK (from the checkout until the first PyPI release):

```bash
cd sdk/python && uv sync && uv run python -c "
import vmlayer
with vmlayer.Sandbox.create('alpine:3.20') as sb:
    print(sb.exec('uname -a').output)
"
```

TypeScript (same API, `await using` cleans up):

```ts
import { Sandbox } from "vmlayer";
await using sb = await Sandbox.create("python:3.12-alpine");
console.log((await sb.exec("python -c 'print(6*7)'")).stdout);
```

See [docs/usage.md](docs/usage.md) for the SDK guide, [examples/](examples/)
for runnable programs, and [docs/deploy.md](docs/deploy.md) for running the
daemon as a service.

## Why this exists

Every team building an agent runtime ends up writing the same thing: a way to
boot an isolated environment, run commands in it, move files in and out, and
throw it away. Most reach for Docker, then discover they want real isolation,
memory snapshots and forking, and start over on Firecracker.

The pieces exist but there is nothing in the middle:

| Layer | What's there | Problem |
|---|---|---|
| VMMs | Firecracker, libkrun, cloud-hypervisor | Too low-level. You still write networking, images, agent, lifecycle. |
| Hosted sandboxes | E2B, Modal, Daytona, Vercel Sandbox... | You rent them. Self-hosting is their full stack or nothing. |
| Local microVM libs | microsandbox, smolvm | One VMM each, no daemon mode, no Firecracker. |

vmlayer is the missing middle: a **pluggable-backend sandbox runtime** that an
agent platform can depend on the way it depends on a database driver. It was
checked against the sandbox interfaces of six open-source agent runtimes
([docs/integrations.md](docs/integrations.md)) and the systems research on
microVM snapshots ([docs/research.md](docs/research.md)).

## Design

The agent loop, credentials, and model calls can stay in the host control
plane. A framework adapter sends only sandbox tool calls to `vmlayerd`, which
creates a Firecracker microVM directly on a KVM host. Kubernetes may schedule
and restart those hosts, but a sandbox create does not create a Kubernetes pod
or put the control plane in a VM. OpenHands is an optional second pattern:
its agent server runs inside the sandbox when that framework needs it.

Head-to-head numbers against the sandboxes that mini-swe-agent and AutoGen
ship are in the
[benchmarks](docs/benchmarks.md#each-frameworks-own-sandbox-versus-the-same-framework-on-vmlayer).

The local [Agent Sandbox + gVisor comparison](docs/benchmarks.md) measures
the pod-per-sandbox alternative. Kubernetes can use VM-backed RuntimeClasses
such as Kata; the distinction here is the sandbox request path and lifecycle,
not a claim that Kubernetes cannot run VMs.

For tools that need no mounted workspace volume, the Firecracker backend can
restore each new VM from an on-disk, data-free boot template. This is optional
(`firecracker.template_snapshots = true`) and keeps no idle VM alive. See the
[cold-path benchmarks and limits](docs/benchmarks.md#on-demand-vms-from-an-on-disk-boot-template).

```
  Python SDK / CLI / (TypeScript)        your agent runtime links one of these
              │  gRPC (unix socket, or TCP + token)
              ▼
  ┌──────────────────────────────────┐
  │  vmlayerd                        │   one binary per host
  │  · sandbox table (SQLite)        │
  │  · volumes: clone, snapshot      │   copy-on-write where the FS allows
  │  · process buffers, TTLs         │
  │  · picks a Backend               │
  └──────────────┬───────────────────┘
                 │  Backend trait  (core/)
     ┌───────────┼─────────────┐
     ▼           ▼             ▼
 firecracker   libkrun       docker
 Linux + KVM   macOS, Linux  anywhere
 production    laptop        CI
     │
     ▼
 guest agent (vsock): exec, files, pty, mounts
```

Rules the design holds itself to:

1. **The unit is a sandbox, never a VM.** Callers do not see vCPUs, tap devices
   or rootfs paths.
2. **Same API on every backend.** What a backend cannot do is declared in
   `Capabilities` and refused with an error. Never approximated.
3. **Snapshot / restore / fork are the point.** They are the reason to pick a
   microVM over a container, so they shape the API instead of being bolted on.
4. **Thin SDKs, one daemon.** Policy lives in `vmlayerd`. SDKs are generated
   clients plus ergonomics.
5. **Images resolved in one place.** Backends receive a `Rootfs`, never an OCI
   reference.
6. **Root disk and workspace are separate.** The root disk is disposable; work
   lives on a `Volume` that can outlive the sandbox, be shared read-only, be
   checkpointed cheaply, and is cloned (never reattached) on restore and fork.
   Anonymous by default, so most callers never see it.
7. **Cloud-agnostic.** Linux + KVM is the only hard dependency for microVMs;
   no cloud APIs, no managed services. See [decision 0005](docs/decisions/0005-cloud-agnostic.md).

The contract lives in two places: [`sdk/python/vmlayer/`](sdk/python/vmlayer/)
(what callers see) and [`core/src/backend.rs`](core/src/backend.rs) (what a
backend implements). Every non-obvious choice has a record in
[docs/decisions/](docs/decisions/README.md).

## Backends

| | Firecracker | libkrun | Docker |
|---|---|---|---|
| Runs on | Linux + KVM | macOS (Apple Silicon), Linux | Anywhere dockerd runs |
| Isolation | microVM, own kernel | microVM, own kernel | container |
| Memory snapshot | yes | yes | no (filesystem only) |
| Fork | yes | planned | no |
| Status | working on aarch64 nested KVM | planned | **working** |

`Backend.AUTO` picks the strongest one the host supports.

## Where it runs

vmlayer depends on Linux and, for microVM backends, `/dev/kvm`. Nothing
cloud-specific: no metadata service, no VPC API, no managed database. State
is SQLite; snapshots are on local disk, and durable volume snapshots go to
any S3-compatible store, Azure Blob, GCS or a shared directory.

| Host | Firecracker backend | Notes |
|---|---|---|
| Bare metal (any provider, on-prem) | yes | best performance |
| AWS C8i / M8i / R8i, `.metal` | yes | nested virt on standard instances since Feb 2026 |
| GCP, nested virt enabled | yes | per-instance flag |
| Azure v3+ families | yes | Hyper-V nesting |
| OCI, select VM shapes or BM | yes | |
| Hetzner Cloud | yes | cheapest dev box |
| Alibaba ECS Bare Metal | yes | standard ECS: no KVM, Docker fallback |
| Any Linux without KVM | Docker fallback | same API, container isolation |
| macOS (Apple Silicon) | libkrun backend (planned) | Docker meanwhile |

## Repository layout

```
proto/              vmlayer.v1 gRPC contract (the wire API)
core/               Backend trait, shared types, errors; no implementation
daemon/             vmlayerd
backends/docker/    Docker backend
backends/firecracker/  Firecracker backend (jailer, netns, vsock)
guest-agent/        static PID 1 agent for microVMs
procs/              process table shared by daemon and agent
cli/                vmlayer command
sdk/python/         Python SDK and end-to-end tests
sdk/typescript/     TypeScript SDK and end-to-end tests
examples/           runnable examples
docs/decisions/     design records     docs/research.md   papers that shaped it
docs/integrations.md  six agent runtimes mapped onto the API
```

## Non-goals

Billing, multi-tenant user management, a web UI, service mesh. Those make a
product. vmlayer is a library.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), the [code of conduct](CODE_OF_CONDUCT.md)
and the [security policy](SECURITY.md). Changes are listed in
[CHANGELOG.md](CHANGELOG.md).

## License

Apache-2.0.
