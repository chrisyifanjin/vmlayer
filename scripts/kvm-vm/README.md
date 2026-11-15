# A KVM host for development

The Firecracker backend needs Linux with `/dev/kvm`. These scripts give you
one on a laptop, so the microVM path can be developed and tested without a
cloud machine.

| Script | Runs on | Does |
|---|---|---|
| `up.sh` | host | starts a Lima VM with nested virtualization, runs the two scripts below in it, forwards the daemon socket to `~/.vmlayer/kvm.sock` |
| `provision.sh` | inside the VM | build tools, Firecracker + jailer (latest release), a guest kernel from Firecracker's CI bucket, builds `vmlayerd`, `vmlayer` and the static guest agent from the mounted repository |
| `run-daemon.sh` | inside the VM | writes a config enabling the Firecracker backend and (re)starts `vmlayerd` as root |

```bash
scripts/kvm-vm/up.sh
export VMLAYER_ENDPOINT=unix://$HOME/.vmlayer/kvm.sock
vmlayer info                                   # firecracker  ready
vmlayer run python:3.12-alpine 'uname -a'      # a real microVM

cd sdk/python
uv run python ../../bench/stress_snapshots.py  # snapshot/restore/fork under load
uv run python ../../bench/bench.py --label "firecracker, nested KVM"
```

After changing Rust code, rerun `up.sh` (or just `provision.sh` and
`run-daemon.sh` through `limactl shell vmlayer-kvm`); the build is
incremental.

Requirements: [Lima](https://lima-vm.io) 1.0+. On macOS, nested
virtualization needs Apple Silicon M3 or newer and macOS 15+. On a Linux
host with KVM you do not need the VM at all: run `provision.sh` and
`run-daemon.sh` directly.

Notes:

- The VM mounts only this repository, read-only, and is separate from any
  other Lima or colima instance. `limactl delete -f vmlayer-kvm` removes it.
- Nested virtualization is slower than bare metal; treat latency measured
  here as an upper bound (see `docs/benchmarks.md`).
- `run-daemon.sh` makes the daemon socket world-accessible inside the VM so
  it can be forwarded over SSH. That is for this development VM only.
- Exposed sandbox ports are forwarded to the host automatically by Lima.
