# 0022. A credential proxy on the egress path

Status: accepted, 2026-11-18.

## Context

Agents in sandboxes call model APIs, package registries and SaaS APIs with
keys. Putting those keys in the sandbox's environment means anything the
agent runs can read them: a prompt-injected `curl`, a malicious package, a
copied `.env`. The first external review called a credential proxy its
strongest request (T2.4): the sandbox should never hold real keys.

## Options considered

**TLS interception.** Install a CA in the guest, route all traffic through
an intercepting proxy, and rewrite `Authorization` headers for known hosts.
It is transparent to clients but intrusive. The CA must enter every
image's trust store, which differs per distribution and per language
runtime (Python's certifi, Node's bundled store, Java keystores).
Certificate pinning breaks it, and a forged CA in the guest is a
liability of its own.

**Placeholders and base URLs, no interception.** Almost every API SDK
takes a base-URL setting (`OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, the
registry URL of pip and npm). Point it at a proxy on the host, give the
sandbox a placeholder where the key would go, and have the proxy swap in
the real key and forward over HTTPS. Nothing in the guest changes but two
environment variables.

**Fly-style tokenizer** (sealed secrets that only the proxy can open).
The guest would hold a sealed blob, which is stronger than a placeholder
only if the guest may choose among many secrets. Here the operator
already decides per sandbox, so a placeholder carries the same authority
with less machinery.

The second option is the choice.

## Decision

- **Configuration.** The operator lists credentials under
  `[[credential_proxy.credentials]]`. Each entry has:
  - `name`, which is also the first path segment on the proxy;
  - `upstream`;
  - `header` and a `value` template (`Bearer {secret}`) for the secret;
  - `secret_env` or `secret_file`, where the daemon reads the secret;
  - `guest_env`, the environment the sandbox gets, with `{url}` and
    `{placeholder}` substituted;
  - optionally `tenants` allowed to use it (decision 0020).
  `[credential_proxy] listen` sets where the proxy serves. The daemon
  checks every entry at startup, including that each secret is readable.
- **Creating a sandbox.** `credentials = ["name", ...]` on create gives the
  sandbox one random placeholder (`vmlayer-<128 random bits>`) and the
  configured variables. The daemon stores the placeholder's SHA-256 with
  the granted names.
- **Reaching the proxy.** Every sandbox reaches the host as
  `vmlayer.host`. On Docker that is `--add-host vmlayer.host:host-gateway`.
  On Firecracker the guest agent writes the VM's own veth peer into
  `/etc/hosts` on every create, restore and fork. The host firewall opens
  exactly the proxy port to guests, also under an egress allowlist; every
  other host port stays closed (decision 0012).
- **Serving a request.** The proxy takes the credential name from the path
  and finds a header whose value (or `Bearer` value) hashes to a
  placeholder granted that name. It removes that header, sets the real
  one, drops hop-by-hop headers, and forwards the method, path, query and
  body to the upstream, streaming both ways (server-sent events included).
  A request with no granted placeholder gets `401`.
- **Lifetime.** The placeholder is also kept in the sandbox's environment
  (`VMLAYER_CREDENTIAL_TOKEN`), and the names in a label. A fork or a
  restore grants the same credentials to the new sandbox, whose restored
  processes still hold that placeholder. Grants are deleted with their
  sandbox; a placeholder stops working when the last sandbox holding it
  is gone.

## Consequences

- Code in the sandbox can use an API but cannot take its key. A leaked
  placeholder works only where the proxy is reachable, only for the
  credentials its sandbox was granted, and only while that sandbox lives.
- The operator controls each credential's upstream, so a placeholder
  cannot be spent against another host. Upstreams may be plain `http://`
  for internal services.
- Clients must honour a base URL. Tools that hard-code a host need the
  operator's DNS or a wrapper; TLS interception may come later for them,
  as an opt-in.
- The proxy runs in the daemon. While the daemon restarts, API calls from
  sandboxes fail and are retried by clients; sandboxes keep running.
- The proxy listens where sandbox traffic arrives (`0.0.0.0` by default in
  the examples). Firewall the port from outside networks: a placeholder
  is a bearer credential.
- Per-request metering and rate limits per sandbox fit naturally here and
  are future work.
