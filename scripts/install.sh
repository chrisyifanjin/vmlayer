#!/usr/bin/env sh
# Install vmlayerd and vmlayer from the latest GitHub release.
#   curl -fsSL https://raw.githubusercontent.com/chrisyifanjin/vmlayer/main/scripts/install.sh | sh
# Set VMLAYER_VERSION to pin a tag, VMLAYER_INSTALL_DIR to change the target.
set -eu
repo="chrisyifanjin/vmlayer"
dir="${VMLAYER_INSTALL_DIR:-/usr/local/bin}"
os="$(uname -s)"; arch="$(uname -m)"
case "$os-$arch" in
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  *) echo "unsupported platform: $os $arch (build from source: cargo build --release)" >&2; exit 1 ;;
esac
if [ -z "${VMLAYER_VERSION:-}" ]; then
  VMLAYER_VERSION="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$VMLAYER_VERSION" ] || { echo "no release found; build from source" >&2; exit 1; }
fi
url="https://github.com/$repo/releases/download/$VMLAYER_VERSION/vmlayer-$VMLAYER_VERSION-$target.tar.gz"
tmp="$(mktemp -d)"
echo "downloading $url"
curl -fsSL "$url" | tar -xz -C "$tmp"
# vmlayer-shim must sit next to vmlayerd (the per-microVM helper, decision 0019).
bins="$tmp/vmlayerd $tmp/vmlayer"
[ -f "$tmp/vmlayer-shim" ] && bins="$bins $tmp/vmlayer-shim"
if [ -w "$dir" ]; then install -m 0755 $bins "$dir/"; else sudo install -m 0755 $bins "$dir/"; fi
rm -rf "$tmp"
echo "installed vmlayerd, vmlayer and vmlayer-shim $VMLAYER_VERSION to $dir"

# What this host can run. Nothing here changes the host.
if [ "$os" = Linux ]; then
  check() { if eval "$2" >/dev/null 2>&1; then echo "  ok    $1"; else echo "  no    $1${3:+: $3}"; fi; }
  echo "host checks:"
  check "/dev/kvm (Firecracker backend)" "test -r /dev/kvm -a -w /dev/kvm" \
    "no KVM; the Docker backend still works (enable nested virtualization or use bare metal)"
  check "vhost_vsock module (guest agent link)" "test -e /dev/vhost-vsock || modinfo vhost_vsock" "modprobe vhost_vsock"
  check "nbd module (lazy volume restore)" "test -e /dev/nbd0 || modinfo nbd" \
    "volumes from the blob store are downloaded before attaching"
  check "cgroup v2 (jailer limits)" "test -f /sys/fs/cgroup/cgroup.controllers"
  check "nftables (microVM networking)" "command -v nft" "install the nftables package"
  check "mkfs.ext4 (images and volumes)" "command -v mkfs.ext4" "install e2fsprogs"
  check "docker (Docker backend)" "docker info" "optional"
fi
echo "start the daemon with: vmlayerd   (see docs/deploy.md for systemd, a container image and Kubernetes)"
