# vmlayerd on Kubernetes

`vmlayer.yaml` runs one `vmlayerd` per KVM node as a DaemonSet, from the
container image built by this repository's `Dockerfile`
(`ghcr.io/chrisyifanjin/vmlayerd`).

```bash
kubectl label node <node> vmlayer.dev/kvm=true
kubectl create namespace vmlayer
kubectl -n vmlayer create secret generic vmlayerd-tokens --from-literal=admin="$(openssl rand -hex 32)"
kubectl apply -f deploy/kubernetes/vmlayer.yaml
```

Clients connect to `tcp://<node-ip>:7070` with the token, or put a
`vmlayer.Cluster` on any node to place sandboxes across all of them once a
shared blob store is configured.

## What the pod needs, and why

| Setting | For |
|---|---|
| `privileged: true` | `/dev/kvm`, `/dev/net/tun` and `/dev/nbd*`; creating network namespaces, veth pairs and nftables rules; the jailer's chroot, `mknod` and cgroups |
| `hostNetwork: true` | each microVM's veth pair, NAT and firewall rules live in the node's network namespace |
| `hostPID: true` | the jailer and per-VM shims run as host processes, as on a VM host |
| `/run/netns` (bidirectional) | per-VM network namespaces must be visible on the node |
| `/var/lib/vmlayer` | sandbox state, volumes, images and snapshots survive pod restarts |
| `/lib/modules` (init) | loads `nbd` for lazy volume restore; without it volumes are downloaded before attaching |
| node label `vmlayer.dev/kvm` | schedule only where `/dev/kvm` exists (nested virtualization or metal) |

These are the privileges of a hypervisor host. Run the DaemonSet on
dedicated node pools, not next to unrelated workloads.

## Restarts

Each microVM's shim (decision 0019) keeps it running when `vmlayerd` exits.
But replacing the pod stops the container's processes, including the
shims, so the node's microVMs stop with it. That is why the update strategy
is `OnDelete`:

1. Drain the node's sandboxes. Rescheduled sandboxes (`reschedule=True`)
   come back on other nodes.
2. Delete the pod.
3. Repeat for the next node.

A daemon crash inside a running pod is not a pod replacement: the
container restarts in place, the shims and VMs keep running, and the new
daemon re-attaches to them.

## Scraping

The pod serves Prometheus metrics on port 9464 (`[metrics] listen`) and
carries the `prometheus.io/scrape` annotations.
