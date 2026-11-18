# 0020. Callers, tenants and TLS

Status: accepted, 2026-11-18. Extends decision 0007 (unix socket by
default, TCP with one bearer token).

## Context

The first external review asked for three things (A5, T1.1):

- Say plainly whom the daemon trusts.
- Add TLS, and mutual TLS, to TCP listeners.
- Give tokens a tenant and a scope, so one daemon can serve several
  callers that must not see each other's sandboxes.

Until now a TCP listener accepted one shared token over plaintext HTTP/2,
and every holder of that token could do everything. Real deployments put a
control plane in front of the daemon (an agent platform, a CI system, a
notebook service). They need:

- Encryption on the wire without a separate proxy.
- Proof that the caller is their own control plane (mutual TLS).
- Read-only tokens for dashboards.
- One daemon fleet shared by several products or customers, each confined
  to its own resources.

## Decision

### Trust model

- **The unix socket is the operator.** File permissions on the socket
  (mode 0660) decide who connects. Every caller there is the local
  administrator: all tenants, all roles.
- **A TCP caller is whoever holds a token.** The daemon maps each token to
  a principal: a name for logs, an optional tenant, and a role. With
  `[tls] client_ca`, the caller must also present a client certificate
  signed by that CA. Mutual TLS proves the caller is a trusted client; the
  token says which tenant and role it acts as. Both are required.
- **Tenants are a partition, not a security boundary between hostile
  operators.** A tenant cannot see or touch another tenant's sandboxes,
  volumes or snapshots through the API. All tenants share the host's
  kernel (Docker backend) or hypervisor (Firecracker), its disk and its
  blob store, under one operator who can see everything. Isolation
  between guests is the backend's job (decisions 0008 and 0012), whatever
  their tenant.
- **Guests are never trusted.** Nothing in this decision changes that.

### Tokens

Tokens are listed in the config file by their SHA-256, so the file holds
no secrets and can live in a config map:

```toml
[[auth.tokens]]
name = "agents-prod"            # for logs
sha256 = "9f86d081..."          # printf %s "$TOKEN" | sha256sum
tenant = "acme"                 # optional; unset = every tenant
role = "write"                  # read | write | admin (default write)
```

`--token` stays: one administrator token with no tenant.

Roles are ordered, and each includes the ones before it:

- `read`: get, list, read files, read process output, wait.
- `write`: everything that creates, changes or deletes sandboxes, volumes
  and snapshots, and runs commands.
- `admin`: host-wide operations, today the blob store's garbage
  collection.

The role an RPC needs is one table keyed by gRPC method
(`auth::required_role`). An RPC missing from the table needs `write`, so
adding an RPC never makes it readable by accident. A middleware in front
of every service checks the token and the role before any handler runs.

### Tenancy

A tenant is a reserved label, `vmlayer.tenant`, on sandboxes, volumes,
volume snapshots and sandbox snapshots. The alternative, a column per
table, needs a schema migration and does not reach the blob store, where
manifests already carry labels. With a label:

- A tenant's creates stamp it, overriding any value the caller sent.
- A tenant's lists filter on it, through the label selectors the list
  RPCs already have.
- A tenant's lookups by ID check it. A resource of another tenant gives
  exactly the error a missing ID gives, so IDs cannot be probed.
- Resources made from others inherit it: forks, restores (and the volumes
  cloned for them), volume clones, volumes from snapshots, and snapshots
  of sandboxes and volumes. An unscoped caller (the operator, or a
  control plane with an unscoped token) acting on a tenant's sandbox
  therefore creates the tenant's resources, not its own.
- Durable snapshots carry the label in their manifests, so the check holds
  for a snapshot pushed by another daemon of the cluster.

Volume names are unique per tenant: the daemon stores `acme/ws`, and a
caller of tenant `acme` creates, finds and sees `ws`. The operator sees and
uses the stored name.

Tenants do not see host layout either. `Info` omits the data directory and
blob store URL, and `ListHosts` omits sandbox IDs.

### TLS

`[tls] cert`, `key` and optionally `client_ca` (or `--tls-cert`,
`--tls-key`, `--tls-client-ca`) turn on TLS for TCP listeners, through
tonic's rustls integration. With `client_ca`, a client certificate is
required. Plaintext TCP still works for loopback and private overlays, with
a warning on a non-loopback address.

Clients use one convention in every SDK and the CLI. A `tls://host:port`
endpoint turns TLS on. Options (`ca`, `cert`, `key`) or the environment
variables `VMLAYER_TLS_CA`, `VMLAYER_TLS_CERT` and `VMLAYER_TLS_KEY` add a
private CA and a client certificate. Advertising `tls://` in the host
registry makes cluster clients reach every host the same way.

## Consequences

- One daemon or fleet can serve several products or customers with
  separate tokens. A leaked read token cannot run code, and a leaked
  tenant token reaches one tenant only.
- No reverse proxy is needed for encryption. Proxies still work in front
  of plaintext listeners.
- Tokens are bearer secrets. Rotation means adding the new token's hash,
  moving callers, removing the old hash, and restarting the daemon.
  Reloading without a restart is future work.
- Tenancy is enforced in the gRPC layer. Internal tasks (rescue,
  checkpoints, garbage collection) act as the operator and propagate the
  label through inheritance. A new RPC that takes an ID must call the
  tenant check. The pattern is one call per handler (`Services::sandbox`,
  `volume`, `snapshot_labels`, `volume_snapshot_labels`), and the live
  tests cover each kind of resource.
- Exposed ports are still raw TCP on the host and are not covered by
  tokens. Authenticated HTTPS expose is tracked separately (review T2.3).
