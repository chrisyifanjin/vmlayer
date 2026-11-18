# 0005. Cloud-agnostic: depend on Linux + KVM, nothing else

Status: accepted, 2026-11-14

## Context

The project must run on any cloud or on-prem, not just the two clouds the
incumbent (E2B) supports via Terraform. The one hard requirement of the
Firecracker backend is a host that exposes `/dev/kvm`.

## Evidence (Nov 2026)

Where `/dev/kvm` is available to a tenant:

| Provider | KVM on regular VMs | Bare metal | Notes |
|---|---|---|---|
| AWS | yes, since Feb 2026 on C8i / M8i / R8i | `.metal` | E2B's own default node is m8i.4xlarge with nested virt |
| GCP | yes, per-instance flag (`enableNestedVirtualization`) | yes | Firecracker's dev docs use a nested-KVM GCE VM |
| Azure | yes on v3+ Dv/Ev and compute families | yes | Hyper-V nesting |
| OCI | select VM shapes | yes | Oracle measured ~3% CPU gap vs BM for Firecracker |
| Hetzner Cloud | yes, all CX/CPX shapes | dedicated servers | cheapest dev host |
| Alibaba Cloud | not on standard ECS | ECS Bare Metal (ebm) | |
| DigitalOcean | unverified | — | one source lists it, no details found |

Nested KVM costs roughly 3–10% CPU depending on workload (Oracle vs Google
figures). E2B's self-host guide requires bare metal or nested virt and only
ships Terraform for GCP (stable) and AWS (beta); Azure and plain Linux are
unchecked boxes on their roadmap.

Takeaway: the host matrix is wide and getting wider, so vmlayer should not
encode any cloud's shape; it should probe the host and otherwise stay out of
the way.

## Decision

vmlayer treats the host as "a Linux machine", full stop. Concretely:

1. **Hard dependency is Linux + `/dev/kvm` for microVM backends.** Nothing
   else: no cloud metadata service, no VPC API, no IAM. Where KVM is absent
   `Backend::Auto` falls back to Docker so the same deployment still works.
2. **No managed services required.** State in SQLite on local disk; snapshots
   and image cache on local disk by default.
3. **Durable storage behind one interface.** Implemented in ADR 0011 with
   the `object_store` crate: local or shared directory, S3 and
   S3-compatible stores (AWS S3, Cloudflare R2, MinIO, Ceph, Alibaba OSS,
   Hetzner Object Storage), Azure Blob, and Google Cloud Storage.
4. **Networking uses Linux primitives only**: tap, bridge, nftables or eBPF
   for NAT and egress policy, and optionally a CNI plugin. Exposing a port
   means a host port, nothing more. Load balancers and public ingress are the
   operator's business.
5. **Packaging is cloud-neutral**: release binaries for Linux and macOS plus
   a systemd unit (`docs/deploy.md`), a container image (`Dockerfile`) and a
   Kubernetes DaemonSet (`deploy/kubernetes/`). Only the guest agent is a
   static musl binary, since it runs inside arbitrary images. The installer
   checks the host for KVM, the `vhost_vsock` and `nbd` modules, cgroup v2
   and nftables, and reports what it found. No Terraform in the core repo; per-cloud examples
   can live in `examples/` later.
6. **Host probe at startup.** `Backend::probe()` (already in the trait) decides
   the `Auto` preference list from what the machine actually exposes, never
   from an environment name.

## Consequences

- A single-node install is `curl | sh` on any Linux box with KVM, including a
  laptop running Linux, a Hetzner VM, or an AWS metal instance.
- Multi-host (the optional cluster layer) must also avoid cloud APIs: node
  discovery is static config or gossip, not cloud tags.
- GPU passthrough, when it comes, will inherit the same rule: VFIO on the
  host, no cloud-specific device plugins.

Sources: AWS nested virtualization announcement coverage (Feb 2026),
Firecracker `docs/dev-machine-setup.md`, Oracle "Firecracker microVMs on OCI"
blog, E2B `infra/self-host.md`.
