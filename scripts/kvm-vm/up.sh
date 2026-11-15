#!/usr/bin/env bash
# Run on the HOST (macOS on Apple Silicon M3+, or Linux): start a small Lima
# VM with nested virtualization, provision it, start vmlayerd inside with the
# Firecracker backend, and forward its socket to the host.
#
#   scripts/kvm-vm/up.sh
#   export VMLAYER_ENDPOINT=unix://$HOME/.vmlayer/kvm.sock
#   vmlayer info          # backend: firecracker
#
# The VM mounts only this repository, read-only. It does not touch any other
# Lima or colima instance. Remove it with: limactl delete -f vmlayer-kvm
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
NAME="${VMLAYER_KVM_VM:-vmlayer-kvm}"
SOCK="${VMLAYER_KVM_SOCK:-$HOME/.vmlayer/kvm.sock}"
command -v limactl >/dev/null || { echo "limactl not found: install Lima (https://lima-vm.io)" >&2; exit 1; }

if ! limactl list -q 2>/dev/null | grep -qx "$NAME"; then
  limactl start --name="$NAME" --vm-type=vz --nested-virt \
    --cpus="${VMLAYER_KVM_CPUS:-4}" --memory="${VMLAYER_KVM_MEMORY:-4}" --disk="${VMLAYER_KVM_DISK:-20}" \
    --set ".mounts=[{\"location\":\"$REPO\",\"writable\":false}]" \
    --tty=false template:ubuntu-24.04
else
  limactl start "$NAME" 2>/dev/null || true
fi
limactl shell "$NAME" test -e /dev/kvm || {
  echo "/dev/kvm is missing in the VM: nested virtualization needs an M3 or newer and macOS 15+" >&2
  exit 1
}
limactl shell "$NAME" env REPO="$REPO" bash "$REPO/scripts/kvm-vm/provision.sh"
limactl shell "$NAME" bash "$REPO/scripts/kvm-vm/run-daemon.sh"

mkdir -p "$(dirname "$SOCK")"
pkill -f "$SOCK:/run/vmlayer/vmlayerd.sock" 2>/dev/null || true
rm -f "$SOCK"
ssh -F "$HOME/.lima/$NAME/ssh.config" -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes \
  -f -N -L "$SOCK:/run/vmlayer/vmlayerd.sock" "lima-$NAME"
echo
echo "vmlayerd (Firecracker) is reachable at unix://$SOCK"
echo "  export VMLAYER_ENDPOINT=unix://$SOCK"
