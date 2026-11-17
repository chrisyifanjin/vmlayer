# 0017. E2B wire compatibility through a gateway

Status: accepted, 2026-11-17

## Context

E2B's SDK is the most common interface agent code is written against.
The import-level facade (`vmlayer.e2b`) needs a code change. The lowest
adoption cost is the one the E2B SDK itself allows: change the address it
talks to, keep the code.

The E2B SDK talks to two things: a REST control plane (`/v2/sandboxes`,
`/sandboxes/{id}`, timeouts, connect) and, inside each sandbox, an agent
called envd, over Connect RPC (`process.Process`, `filesystem.Filesystem`,
JSON codec) plus `/files` and `/health`. It names the target sandbox in an
`E2b-Sandbox-Id` header on every envd request, and honours
`E2B_API_URL` and `E2B_SANDBOX_URL`.

## Options considered

**Run E2B's envd inside every vmlayer sandbox.** Exact behaviour, but it
adds a second agent to every image and still needs the control plane and
per-sandbox routing.

**Implement the protocols in the Rust daemon.** One process, but it means
hand-maintaining E2B's protobuf and Connect definitions in Rust and
following every SDK release.

**A gateway on vmlayer's Python SDK, using the E2B package's own generated
server code.** The `e2b` package ships the Connect server classes for both
envd services; implementing them on vmlayer calls keeps the wire format
exactly E2B's, version for version. Chosen.

## Decision

`vmlayer.e2b_server` (`vmlayer-e2b-server`, extra `vmlayer[e2b-server]`) is
one HTTP service:

- **Control plane**: create (`templateID`, `timeout`, `metadata`,
  `envVars`, `allow_internet_access`), get, list with metadata filters,
  delete, set timeout, connect. It returns a per-sandbox access token and
  checks it on every envd request; the control plane checks `X-API-Key`.
- **envd**: processes (start with streamed output and exit status, connect,
  list, signals, stdin, close stdin) and the filesystem (stat, list with
  depth, make dir, move, remove, `/files` read and write in multipart or
  octet-stream form, gzip). Commands E2B runs as `/bin/bash -l -c` run with
  `/bin/sh -c` on images without bash. It reports envd 0.5.7, the newest
  feature level it implements.
- **Templates** are OCI image references, or labelled vmlayer snapshots
  (`vmlayer.e2b.create_template`); `base` maps to a configured image.
- **Timeouts** are enforced by the gateway, so `set_timeout` works; the
  vmlayer sandbox also gets a 24-hour TTL as a backstop.

Unsupported operations answer "unimplemented" (Connect) or 501 (REST)
instead of pretending: PTY sessions, filesystem watchers, pause and resume,
E2B snapshots and forks, metrics and logs, and E2B's template build
service.

## Consequences

- Unmodified E2B Python code runs against vmlayer with `E2B_API_URL`,
  `E2B_SANDBOX_URL` and `E2B_API_KEY` set; tested with the sync and async
  Python SDKs (e2b 2.55). The JavaScript SDK speaks the same protocols but
  is untested.
- The gateway follows E2B's protocol through its pinned `e2b` dependency;
  an SDK release that changes the protocol needs a gateway release.
- Plain HTTP: put a TLS-terminating proxy in front for use across
  networks. Sandboxes keep vmlayer ids (`sb-...`), which E2B code treats as
  opaque strings.
- Gateway-enforced timeout extensions are lost if the gateway restarts; the
  initial timeout survives as a label.
