# 0021. An authenticated HTTPS gateway for exposed ports

Status: accepted, 2026-11-18.

## Context

`expose(port)` makes a guest port reachable as a raw TCP port on the host.
That fits a control plane on the same network, which proxies to the port
itself. It does not fit the case the first external review raised (T2.3):
an agent that runs entirely inside the sandbox (an IDE server, an agent
server, a notebook) and that a user or another service must reach over
the internet. Raw host ports there leave four gaps:

- no encryption;
- no authentication, so anyone who finds the port has the agent;
- one host port per sandbox port, which firewalls and load balancers
  handle poorly;
- no stable name that a browser can keep cookies for.

Hosted sandboxes solve this with a wildcard domain:
`<port>-<sandbox>.<domain>` (E2B, Modal, Daytona and others), served by a
proxy that knows the routes.

## Options considered

**Leave it to the operator's reverse proxy.** Every deployment would
rebuild route discovery, per-port tokens and WebSocket handling, and
would have to watch the daemon to learn routes.

**Path-based routing** (`/sandboxes/<id>/ports/<port>/`). It needs no
wildcard DNS, but apps that emit absolute URLs or set root-path cookies
break behind a path prefix, and those are the IDE and notebook servers
this is for.

**A gateway in the daemon, routed by hostname.** It knows the routes, holds
the tokens and needs no extra component. This is the choice.

## Decision

`[gateway]` in the daemon config turns on an HTTP(S) listener. It needs
`listen` and `domain`; `cert` and `key` (a wildcard certificate for
`*.<domain>`) turn on TLS, and `public_port` sets the port written into
URLs when a load balancer sits in front. It is off by default.

- `Expose` takes `gateway = true`. The daemon exposes the port as before,
  creates a random token (two v4 UUIDs, 244 random bits), stores only its
  SHA-256 per (sandbox, port) in SQLite, and returns the URL
  `https://<port>-<sandbox-id>.<domain>` with the token. The token is
  returned only by this call.
- The gateway finds the route from `Host` and checks the token. The token
  may come as `Authorization: Bearer`, as `X-Vmlayer-Token`, or as a
  `vmlayer_token` cookie. A `?vmlayer_token=` query, for opening the URL in
  a browser, sets that cookie (HttpOnly, SameSite=Lax, Secure under TLS)
  and redirects to the same URL without it. Cookies are per hostname, so
  one port's cookie does not unlock another.
- The gateway removes its own credentials before forwarding, so the token
  never reaches the sandbox. The app's own `Authorization` header passes
  through untouched.
- Requests are reverse-proxied over HTTP/1.1 to the port's host endpoint
  from the existing expose path, so the gateway works with every backend.
  Protocol upgrades (WebSocket, and any other `Upgrade`) become a byte
  stream in both directions. Clients may speak HTTP/2 to the gateway.
- An unpublished port and a wrong token get the same `401`, so the
  gateway cannot be probed for sandbox IDs. `Unexpose` and sandbox
  deletion remove the route.
- Tenancy (decision 0020) applies to `Expose`, so a tenant cannot publish
  another tenant's port. A route's token is a capability to reach that one
  port, whoever holds it.

## Consequences

- An agent or IDE inside a sandbox is reachable over HTTPS with one DNS
  record and one certificate for the whole fleet.
- Routes and token hashes survive daemon restarts. The gateway itself
  runs in the daemon, so it is down while the daemon is; the sandbox and
  its raw exposed port keep running.
- Each request costs a token hash, a SQLite lookup and a new upstream
  connection. That is fine for interactive traffic. Connection pooling
  can come if measurements ask for it.
- Tokens are bearer secrets with no expiry; re-exposing a port rotates its
  token. Short-lived signed URLs are possible future work.
