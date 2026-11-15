# Local benchmark: Firecracker on nested KVM

Measured on 15 November 2026 with Firecracker 1.18.0 in a 4 vCPU, 4 GiB
Ubuntu 24.04 Lima VM on an Apple M5. The client and daemon both ran inside
that VM. The image was `python:3.12-alpine`, already pulled, with one vCPU
and 256 MiB per sandbox. Times include the vmlayer SDK and daemon round trip.
They are **nested virtualization** measurements, not bare-metal claims.

| Operation | Median | p95 | Samples |
|---|---:|---:|---:|
| Create | 1,332 ms | 1,408 ms | 10 |
| Create then first `true` | 1,433 ms | 1,511 ms | 10 |
| `true` in a running sandbox | 2.8 ms | 3.9 ms | 100 |
| Start Python and run `pass` | 9.3 ms | 591 ms | 10 |
| Write / read 1 MiB | 8.9 / 5.7 ms | 45.6 / 25.4 ms | 10 each |
| Snapshot memory and disks | 375 ms | 518 ms | 10 |
| Restore then first `true` | 296 ms | 394 ms | 10 |
| Fork four children | 1,359 ms | 1,387 ms | 5 |
| Create four concurrently, total | 5,475 ms | 5,630 ms | 3 |
| Kill | 20 ms | 27 ms | 10 |

The full samples and machine labels are in
[`bench/results/firecracker-nested-m5-2026-11-15.json`](../bench/results/firecracker-nested-m5-2026-11-15.json).
The 591 ms Python p95 is one outlier among ten runs; more samples are needed
before drawing a tail-latency conclusion. An earlier eight-way parallel run
timed out one cold boot at the 30 second guest-agent readiness limit on this
4 vCPU VM. That run is excluded from the table; use a larger host or reduce
parallelism before treating eight-way throughput as representative.

Reproduce from inside a KVM host after `scripts/kvm-vm/provision.sh` and
`scripts/kvm-vm/run-daemon.sh`:

```bash
export VMLAYER_ENDPOINT=unix:///run/vmlayer/vmlayerd.sock
export PYTHONPATH=/path/to/vmlayer/sdk/python
python bench/bench.py --image python:3.12-alpine --runs 10 \
  --exec-runs 100 --fork 4 --parallel 4 --memory-mb 256 \
  --label 'Firecracker 1.18, nested KVM (4 vCPU, 4 GiB)' \
  --json bench/results/firecracker-local.json
```

`bench/bench.py` pre-pulls the image outside the timed sections. Cold image
pull, host setup, and daemon launch are excluded. Snapshot size is the
logical memory plus root disk size (512 MiB here), not the physical blocks
consumed by reflink copies.

## Agent Sandbox with gVisor versus direct Firecracker

The same Apple M5 ran two 4-vCPU, 4-GiB Linux environments. Minikube used
Docker Desktop, containerd, the Agent Sandbox v1.0.2 controller, and gVisor
`runsc` for one pod per sandbox. vmlayer used Firecracker 1.18 in Lima with
nested KVM and one VM per sandbox. Both sides ran a cached ARM64
`python:3.12-alpine` image with a 1-CPU, 256-MiB workload limit. Each row has
10 alternating, sequential samples after a warmup. Timings are client wall
clock, including control-plane and transport overhead; image pull is excluded.

| Operation | Agent Sandbox + gVisor median / p95 | vmlayer + Firecracker median / p95 |
|---|---:|---:|
| Create until ready | 1,105 / 2,013 ms | 1,531 / 1,699 ms |
| First `true` | 67 / 86 ms | 92 / 104 ms |
| First Python process | 80 / 101 ms | 664 / 713 ms |
| Second Python process | 64 / 99 ms | 65 / 76 ms |
| Delete until gone | 1,745 / 1,797 ms | 24 / 34 ms |

The Kubernetes measurements call `kubectl apply`, `wait`, `exec`, and `delete`;
the vmlayer measurements use the Python SDK over a forwarded Unix socket.
`kubectl` process startup contributes to the Kubernetes numbers, especially
exec and delete. A production Agent Sandbox client would use the Kubernetes
API directly. These data measure two complete local request paths, **not**
the isolated cost of gVisor against Firecracker. The two Linux hosts have
different virtualization stacks and storage. Ten serial trials do not
establish throughput or behavior at scale.

The first Python process in the new Firecracker VM incurs guest cold-page
costs that disappear on the second call. This explains why the 9.3 ms
steady-state Python median in the earlier table must not be compared with
the 80 ms first-call gVisor number. These rows measure ordinary cold boot;
the optional template path is measured separately below.

The full samples are in
[`bench/results/agent-sandbox-gvisor-vs-vmlayer-firecracker-2026-11-15.json`](../bench/results/agent-sandbox-gvisor-vs-vmlayer-firecracker-2026-11-15.json).
Reproduce with `bench/compare_agent_sandbox.py`; it requires a running
Minikube Agent Sandbox controller, a `gvisor` RuntimeClass, and a vmlayer
daemon on a KVM host. The script uses a new Kubernetes Sandbox custom
resource and a new vmlayer sandbox on each trial, then cleans both up.

### Follow-up: pre-warmed paths

A separate five-run probe on the same Minikube and Lima environments measured
the two available pre-warmed mechanisms. For vmlayer, we booted one VM, ran
Python once, saved a memory snapshot, then restored a new VM and ran Python
once in each trial. For Agent Sandbox, we kept one ready gVisor pod in a
`SandboxWarmPool`, created a `SandboxClaim` to adopt it, then ran Python once
through `kubectl exec`. We waited for the pool to refill before each claim.

| Warm path, median of 5 | Allocate / restore | First Python | Total |
|---|---:|---:|---:|
| Agent Sandbox + gVisor warm-pool claim | 177 ms | 72 ms | 249 ms |
| vmlayer + Firecracker memory-snapshot restore | 192 ms | 486 ms | 678 ms |

These are exploratory results, not a matched resource-cost comparison. The
Agent Sandbox pool keeps a pod running while idle; the vmlayer snapshot is a
disk artifact. A vmlayer pool of running VMs has not been implemented. The
vmlayer template was warmed by one Python call, but the first process after
restore still incurred memory page faults as the VM resumed from its snapshot.
The rounded client samples, in milliseconds, were:

| Path | Per-trial total |
|---|---|
| Agent Sandbox warm claim + Python | 267, 249, 243, 237, 253 |
| Firecracker restore + Python | 706, 678, 661, 672, 693 |

The existing controller's warm-pool path is faster in this local latency
probe, at the cost of keeping a pod running. The on-demand template comparison
below uses the direct Kubernetes API, without a warm pool.

### On-demand VMs from an on-disk boot template

An optional Firecracker backend setting, `template_snapshots = true`, now
builds a data-free template after the first cold create for an image and VM
shape. The build runs in the background. It boots a separate VM, optionally
runs an administrator-specified `template_warmup_argv`, snapshots it, and kills
it. Later creates restore a **new** microVM from that disk snapshot and apply
their own hostname, environment, workdir, and entrypoint. Sandboxes with
mounted volumes use the ordinary cold boot path. The default setting is off;
the Lima development script enables it with a Python warmup command. A
template is a prepared *disk image of memory*, not a pool of idle VMs.

The same Minikube gVisor and Lima Firecracker hosts were measured again with
the direct Kubernetes API and vmlayer Python SDK. Both images were cached.
Each trial created a new sandbox, ran Python, then deleted it. The Firecracker
side used no workspace volume so it could use the template. The gVisor side
used no warm pool. Ten sequential trials followed one warmup per side.

| Operation, p50 / p95 | Agent Sandbox + gVisor | vmlayer + Firecracker template |
|---|---:|---:|
| Create until ready | 1,161 / 1,443 ms | 327 / 381 ms |
| Create through first Python | 1,207 / 1,492 ms | 642 / 718 ms |
| First Python after create | 43 / 51 ms | 322 / 348 ms |
| Delete until gone | 1,947 / 2,918 ms | 21 / 35 ms |

Four simultaneous creates per batch (three batches, 12 trials per backend)
gave a **batch completion** median of 3,338 ms for gVisor and 3,187 ms for
Firecracker. Individual create-through-Python p50 was 1,814 ms for gVisor and
2,806 ms for Firecracker. The 4-vCPU nested-KVM host is saturated here: this
small experiment does not establish greater throughput or better p95 latency.

The idle template uses no VMM process or guest RAM. Its memory file occupied
about 257 MiB on this host for the 256-MiB test shape, plus root disk and VM
state; template files currently need manual pruning as image versions and
settings change. A running daemon, KVM host, and Kubernetes or host scheduler
still cost resources. The first cold request also pays the normal boot cost,
while a background template build briefly consumes an extra VM's resources.
These local results compare full request paths on different Linux hosts, so
they are directional rather than an isolation-adjusted cost benchmark.

Samples and settings:
[`serial`](../bench/results/agent-sandbox-gvisor-vs-vmlayer-firecracker-template-python-warm-2026-11-15.json),
[`four-way`](../bench/results/agent-sandbox-gvisor-vs-vmlayer-firecracker-template-python-warm-parallel4-2026-11-15.json).
Reproduce with `bench/compare_agent_sandbox.py --transport api --no-workspace`
and, for bursts, `--parallel 4`. The script uses the Kubernetes Python client
for create, watch, exec, and delete, avoiding `kubectl` subprocess overhead.
