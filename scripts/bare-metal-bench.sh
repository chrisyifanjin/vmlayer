#!/usr/bin/env bash
# Benchmark vmlayer's Firecracker backend on a bare-metal x86_64 (or arm64)
# Linux host with /dev/kvm, and write the results into bench/results/.
#
#   git clone https://github.com/chrisyifanjin/vmlayer && cd vmlayer
#   sudo apt-get install -y git curl   # Ubuntu 22.04 or 24.04
#   scripts/bare-metal-bench.sh
#
# Runs as a user with sudo. Installs build tools, Firecracker and a guest
# kernel under /opt/vmlayer (scripts/kvm-vm/provision.sh), starts the
# daemon, runs the end-to-end tests, the snapshot stress test and the
# benchmark, and prints where the JSON went. Nothing outside /opt/vmlayer,
# /var/lib/vmlayer and /run/vmlayer is changed, apart from apt packages.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"
[ -e /dev/kvm ] || { echo "no /dev/kvm: this host cannot run microVMs" >&2; exit 1; }
if [ -e /sys/class/dmi/id/sys_vendor ] && grep -qiE 'qemu|kvm|vmware|xen|microsoft|amazon ec2|google' /sys/class/dmi/id/sys_vendor 2>/dev/null; then
  case "$(cat /sys/class/dmi/id/product_name 2>/dev/null)" in
    *metal*) ;;  # e.g. AWS *.metal instances report the cloud vendor
    *) echo "warning: this looks like a virtual machine ($(cat /sys/class/dmi/id/sys_vendor)); results would be nested-virtualization numbers" >&2 ;;
  esac
fi
sudo chmod 0666 /dev/kvm
sudo modprobe nbd nbds_max=64 2>/dev/null || true
REPO="$REPO" scripts/kvm-vm/provision.sh
scripts/kvm-vm/run-daemon.sh
command -v uv >/dev/null || curl -LsSf https://astral.sh/uv/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
export VMLAYER_ENDPOINT=unix:///run/vmlayer/vmlayerd.sock
export VMLAYER_FIRECRACKER_ENDPOINT=$VMLAYER_ENDPOINT
cd sdk/python
uv sync -q --group dev
uv run pytest -q tests/test_firecracker.py
uv run python ../../bench/stress_snapshots.py --restores 20 --forks 10 --children 3
cpu="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ *//')"
label="Firecracker $(cat /opt/vmlayer/firecracker.version), bare metal, ${cpu}, $(nproc) CPUs"
out="$REPO/bench/results/firecracker-bare-metal-$(uname -m)-$(date +%Y-%m-%d).json"
uv run python ../../bench/bench.py --image python:3.12-alpine --runs 20 --exec-runs 200 \
  --fork 8 --parallel 8 --memory-mb 256 --label "$label" --json "$out"
echo
echo "results: $out"
