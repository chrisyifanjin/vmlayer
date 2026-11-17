# 0012. Network isolation, egress allowlists, and IO limits

Status: accepted, 2026-11-17

## Context

Sandboxes run code nobody reviewed. Before this decision a Firecracker
microVM could:

1. Reach the cloud instance metadata service at `169.254.169.254` through
   the host's NAT. On AWS, GCP and Azure that service hands out the host's
   credentials (the classic SSRF-to-cloud-takeover path).
2. Reach any service listening on the host, through the host end of its
   veth pair: sshd, a TCP-exposed Docker or vmlayer API, a database.
3. Reach other microVMs through their link addresses, which the host
   routes.
4. Connect anywhere on the internet. Platforms that run agents need to
   restrict that to package indexes, a model API, a git host.
5. Use as much network and disk bandwidth as the host has, starving
   neighbours.

The Docker backend has the same exposure for 1 and 4 and cannot fix it
without host privileges the daemon may not have (Docker Desktop on macOS,
rootless engines).

## Options considered

**Egress by IP only, resolving names at create time.** Simple, but the
addresses behind CDNs (PyPI is on Fastly, GitHub on its own anycast) change
and differ per resolver. A rule written as `pypi.org` would break within
hours, or need very broad CIDRs.

**An HTTP(S) proxy with SNI or Host filtering** (the approach of several
hosted sandboxes). Strong for HTTP, but it covers only proxy-aware clients,
needs TLS interception or SNI peeking, and does nothing for SSH, database
protocols or plain TCP.

**DNS-driven address sets** (Cilium's FQDN policy, dnsmasq `--nftset`,
Kubernetes NetworkPolicy implementations with FQDN support). A resolver the
guest is forced to use answers only allowed names, and inserts each answer's
addresses into a firewall set with the record's TTL. Works for every
protocol, follows DNS changes, adds no proxy to the data path. Chosen.

**eBPF instead of nftables.** Faster at very high rule counts, but nftables
is present on every Linux that runs Firecracker, sets with timeouts give
exactly the needed semantics, and the rules can be read with `nft list`.
Revisit if per-host rule volume becomes a bottleneck.

**IO limits: cgroups versus Firecracker's rate limiters.** Firecracker has
token-bucket rate limiters on each virtio-net and virtio-block device,
applied by the VMM itself and carried in snapshots. They need no host
cgroup setup. Chosen.

## Decision

Host table (`ip vmlayer`, applied at daemon start):

- Masquerade the VM address pool.
- `input`: from any `vmlveth*` interface accept established traffic and DNS
  to the proxy port, drop everything else. A guest can no longer open a
  connection to the host. Replies to the daemon's own connections (`expose`)
  pass as established traffic.
- `forward`: drop `vmlveth*` to `vmlveth*`. VMs cannot reach each other.

Per-VM namespace table:

- Always drop `169.254.0.0/16` and `168.63.129.16` from the guest, before
  any accept.
- With an egress policy: redirect all guest port-53 traffic (any resolver,
  UDP and TCP) to the VM's DNS proxy; accept established traffic, the
  proxy, the policy's networks (with optional ports), and members of two
  sets filled by the proxy, `dns_any` (address) and `dns_port` (address and
  port); reject the rest (TCP reset, else ICMP admin-prohibited) so blocked
  connections fail at once instead of timing out.

DNS proxy, one per VM with a policy, on the host end of its veth:

- NXDOMAIN for names no rule allows. Rules are exact names (`pypi.org`) or
  subdomain wildcards (`*.pythonhosted.org`, not matching the apex).
- For allowed names: forward to the configured upstream resolvers, add the
  answer's IPv4 addresses to the sets with a timeout of the record TTL,
  clamped to 5 minutes to 24 hours, then reply. If the sets cannot be
  updated it answers SERVFAIL rather than hand out unreachable addresses.

API: `egress=[...]` on create (rules as strings: `10.0.0.0/8`,
`1.2.3.4:5432`, `pypi.org`, `*.pythonhosted.org:443`, `github.com:22,443`;
`[]` blocks all egress); `limits=IoLimits(network_mbps, disk_mbps,
disk_iops)`. Two new capabilities, `egress_policy` and `io_limits`, are true
for Firecracker. The Docker backend refuses both with `BackendUnsupported`
instead of approximating them. Invalid rules are rejected with
`InvalidArgument` before any work is done.

## Consequences

- A guest that holds an address learned through an allowed name can
  connect to that address on the rule's ports for the TTL window, even if
  the address also serves other names. This is inherent to IP-level
  filtering and the same as in Cilium.
- Long-lived connections survive set expiry through conntrack; new
  connections after expiry need a fresh lookup, which clients do.
- IPv6 is not offered to guests, so it needs no rules; IPv6 rules are
  rejected.
- The Docker backend still reaches metadata services and the network
  freely. Run it only for development, or block metadata at the host.
- Boot templates carry device settings, so sandboxes with IO limits use
  the cold boot path.
- The host's own services are unreachable from guests with no exception
  list. A service sandboxes must use should listen elsewhere or be reached
  by an allowed external address.
