# vmlayer

**The layer between agent runtimes and microVMs.**

One sandbox API. Firecracker in production, libkrun on your Mac, Docker in CI.
Snapshot, restore and fork as first-class operations. Not a hosted product: a
daemon plus SDKs you embed in your own platform.

> Status: **step 0**. The API surface and the backend trait are written and
> compile. Nothing runs yet. See [Roadmap](#roadmap).

```python
from vmlayer import Sandbox, Volume

with Sandbox.create("python:3.12", memory_mb=512) as sb:
    sb.files.write("/work/main.py", b"print('hello from a microVM')")
    print(sb.exec("python /work/main.py").stdout)

    snap = sb.snapshot()        # RAM + disk, tens of ms
    kids = sb.fork(4)           # four copies, same state, own identity

sb2 = Sandbox.restore(snap)     # resumes where snapshot() was called

# a workspace that outlives any one sandbox
repo = Volume.create(name="proj-42", size_mb=8192)
with Sandbox.create("python:3.12", workspace=repo) as sb:
    sb.exec("git clone https://example.com/repo .")
ckpt = repo.snapshot(durable=True)          # files only, cheap, portable
with Sandbox.create("node:22", workspace=repo) as sb:
    sb.exec("npm test")                     # same files, different image
```

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
agent platform can depend on the way it depends on a database driver.

## Design

```
  Python / TypeScript / Rust SDKs        your agent runtime links one of these
              │  gRPC (unix socket or TCP)
              ▼
  ┌──────────────────────────────────┐
  │  vmlayerd                        │   one binary per host
  │  · sandbox table, TTLs           │
  │  · image pipeline  OCI → rootfs  │   images resolved once, here
  │  · snapshot store                │
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
 guest agent (vsock): exec, files, pty
```

Rules the design holds itself to:

1. **The unit is a sandbox, never a VM.** Callers do not see vCPUs, tap devices
   or rootfs paths.
2. **Same API on every backend.** What a backend cannot do is declared in
   `Capabilities` and refused with an error. Never approximated.
3. **Snapshot / restore / fork are the point.** They are the reason to pick a
   microVM over a container, so they shape the API instead of being bolted on.
4. **Thin SDKs, one daemon.** Business logic lives in `vmlayerd`. SDKs are
   generated clients plus ergonomics.
5. **Images resolved in one place.** Backends receive a `Rootfs`, never an OCI
   reference.
6. **Root disk and workspace are separate.** The root disk is disposable; work
   lives on a `Volume` that can outlive the sandbox, be shared read-only, be
   checkpointed cheaply, and is cloned (never reattached) on restore and fork.
   Anonymous by default, so most callers never see it.

Read the two files that define the project:

- [`sdk/python/vmlayer/api.py`](sdk/python/vmlayer/api.py): the user-facing surface.
- [`core/src/backend.rs`](core/src/backend.rs): the contract each engine implements.

## Backends

| | Firecracker | libkrun | Docker |
|---|---|---|---|
| Runs on | Linux + KVM | macOS (Apple Silicon), Linux | Anywhere dockerd runs |
| Isolation | microVM, own kernel | microVM, own kernel | container |
| Memory snapshot | yes | yes | no (filesystem only) |
| Fork | yes | planned | no |
| Intended for | production | local development | CI, fallback |

`Backend.AUTO` picks the strongest one the host supports.

## Where it runs

vmlayer depends on Linux and, for microVM backends, `/dev/kvm`. Nothing
cloud-specific: no metadata service, no VPC API, no managed database. State
is SQLite; blobs are local disk or any S3-compatible store.

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
| macOS (Apple Silicon) | libkrun backend (roadmap) | Docker meanwhile |

See [decision 0005](docs/decisions/0005-cloud-agnostic.md) for the evidence
and the rules that keep it this way.

## Roadmap

- [x] **0. API surface.** Python API, Rust `Backend` trait, design records, this README.
- [ ] **1. Vertical slice.** Firecracker on a KVM host boots a prebuilt rootfs
      plus a second ext4 as the workspace volume, guest agent over vsock mounts
      it, `exec("echo hi")` round-trips. Everything else hardcoded.
- [ ] **2. Snapshot, restore, fork.** On the same host, including volume
      clones via reflink. Measure restore latency. Solve vsock re-attach and
      fresh network identity after fork.
- [ ] **3. Second backend.** Docker behind the same trait, to prove the
      abstraction has no leaks.
- [ ] **4. Productize.** `vmlayerd` gRPC server, Python SDK client, OCI → ext4
      pipeline, networking.
- [ ] **5. libkrun backend** for macOS development.
- [ ] Later: TypeScript SDK, E2B-compatible facade
      ([mapping](docs/e2b-mapping.md)), optional multi-host scheduler.

## Non-goals

Billing, multi-tenant user management, a web UI, service mesh. Those make a
product. vmlayer is a library.

## License

Apache-2.0.
