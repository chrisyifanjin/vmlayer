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
echo "start the daemon with: vmlayerd   (see docs/deploy.md for systemd)"
