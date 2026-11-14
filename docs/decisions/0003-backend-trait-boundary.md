# 0003. Backends consume `Rootfs`, never OCI references

Status: accepted, 2026-11-14

## Context

Users want `Sandbox.create("python:3.12")`. Someone has to turn that OCI
reference into something a VMM can boot: an ext4 image for Firecracker, a
directory or image for libkrun, an engine image for Docker.

## Options

1. Each backend resolves images itself. Three implementations of pull, auth,
   layer unpack and caching; three places for bugs.
2. The daemon resolves once into a `Rootfs` enum and hands backends a ready
   artifact. Backends declare which `Rootfs` variants they accept.

## Decision

Option 2. The image pipeline is one component in `vmlayerd`. The `Backend`
trait takes `SandboxSpec { rootfs: Rootfs, .. }` and rejects variants it cannot
use with `Error::Unsupported`.

## Consequences

- Image caching and dedup happen in one place and work across backends.
- A Firecracker-ready ext4 is also what libkrun can boot, so the two microVM
  backends share artifacts.
- The same rule applies to other cross-cutting concerns: the daemon turns a
  shell string into `argv`, enforces TTLs and picks `Backend::Auto`; backends
  stay mechanical.
