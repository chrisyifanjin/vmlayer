# 0002. Daemon plus thin SDKs, not an in-process library

Status: accepted, 2026-11-14

## Context

Agent runtimes are mostly Python and TypeScript. "Library" could mean an
in-process binding (microsandbox's model: the SDK spawns the VMM as a child
process, no daemon) or a host daemon with generated clients (Docker's model).

## Options and evidence

In-process: zero deployment, simplest for a laptop. But Firecracker needs root
or a jailer setup, tap devices, and processes that outlive the caller. Putting
that in someone's Python worker is fragile, and multi-tenant host management
(TTL enforcement, resource accounting, reattach after crash) has no natural
home.

Daemon: one binary per host owns the sandbox table, image cache and snapshot
store; SDKs are gRPC clients. This is what every production Firecracker
runtime surveyed does (E2B, Fly, AerolVM, OpenDsec).

## Decision

Daemon (`vmlayerd`) plus thin SDKs. The Rust core is structured so an
in-process mode can be added later for local development, but the daemon is
the primary form.

## Consequences

- The API surface in `sdk/python/vmlayer/` is the client view; the
  daemon's gRPC schema is derived from it, not the other way round.
- SDK logic is kept to ergonomics (context managers, iterators, type mapping).
  Anything that decides something lives in the daemon.
