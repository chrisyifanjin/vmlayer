# 0001. Rust for the core, daemon and guest agent

Status: accepted, 2026-11-14

## Context

The host-side code has to drive three engines: Firecracker (a separate process
spoken to over a unix-socket HTTP API), libkrun (a C-ABI library linked into
the caller's process, Rust underneath), and Docker (an HTTP API). It also has to
ship a guest agent that runs inside every sandbox.

## Options and evidence

Surveyed what comparable projects use (Nov 2026):

| Layer | Rust | Go |
|---|---|---|
| VMMs | Firecracker, libkrun, cloud-hypervisor | — |
| Embedded microVM libs | microsandbox, smolvm | — |
| KVM-level fork (sub-ms) | Zeroboot | — |
| Guest agents | Fly.io init, Kata agent | E2B envd, OpenSandbox execd |
| Daemons that exec Firecracker | — | E2B orchestrator, AerolVM, OpenDsec, Fly flyd, flintlock, firecracker-containerd |

The pattern: projects whose value is in the orchestration layer and treat the
VMM as an external process are Go; projects whose value is in the VMM layer, or
that link a VMM in-process, are Rust.

Go's concrete advantages: firecracker-go-sdk, the OCI tooling
(go-containerregistry, containerd snapshotters), netlink/CNI libraries, and a
larger pool of infra contributors.

Rust's concrete advantages: libkrun is native (Go needs cgo plus macOS
Hypervisor.framework entitlement handling); any future KVM-level work
(copy-on-write fork below the Firecracker API) is only practical in Rust; a
Rust core can also be exposed in-process via PyO3/napi for a no-daemon local
mode; the guest agent can be a few hundred KB static musl binary.

## Decision

Rust for everything host-side and guest-side. The differentiators of this
project (backend abstraction including libkrun, fork performance) all sit in
the layer where Rust is the established choice.

## Consequences

- OCI image handling needs more hand-written code (`oci-client` for pulls,
  tar/gzip layer unpack, ext4 creation). Budgeted in roadmap step 4.
- The Firecracker API client is written in-house: HTTP over a unix socket,
  small surface.
- Python and TypeScript SDKs stay thin gRPC clients regardless of core language.
