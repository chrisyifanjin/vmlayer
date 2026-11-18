#!/usr/bin/env bash
# Runs INSIDE a Linux VM that has /dev/kvm (see scripts/kvm-vm/README.md).
# Installs build tools, Firecracker + jailer, a guest kernel, and builds
# vmlayerd and the CLI from the repository mounted at $REPO.
set -euo pipefail
REPO="${REPO:?set REPO to the repository path inside the VM}"
ARCH="$(uname -m)"
PREFIX="${PREFIX:-/opt/vmlayer}"

sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  build-essential pkg-config protobuf-compiler curl ca-certificates \
  iproute2 nftables e2fsprogs musl-tools >/dev/null

if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
fi
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
rustup target add "$ARCH-unknown-linux-musl" >/dev/null

sudo mkdir -p "$PREFIX/bin" "$PREFIX/kernel"
sudo chown -R "$(id -u):$(id -g)" "$PREFIX"

PREFIX="$PREFIX" bash "$REPO/scripts/fetch-firecracker.sh"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/target}"
cd "$REPO"
cargo build --release --locked -p vmlayerd -p vmlayer-cli -p vmlayer-backend-firecracker 2>&1 | tail -2
cargo build --release --locked -p vmlayer-guest-agent --target "$ARCH-unknown-linux-musl" 2>&1 | tail -1
# Replace atomically: a running vmlayerd keeps its old inode ("text file busy"
# would otherwise fail the copy and silently leave the old binary in place).
for bin in "$CARGO_TARGET_DIR/release/vmlayerd" "$CARGO_TARGET_DIR/release/vmlayer" \
           "$CARGO_TARGET_DIR/release/vmlayer-shim" \
           "$CARGO_TARGET_DIR/$ARCH-unknown-linux-musl/release/vmlayer-guest-agent"; do
  install -m 0755 "$bin" "$PREFIX/bin/.$(basename "$bin").new"
  mv -f "$PREFIX/bin/.$(basename "$bin").new" "$PREFIX/bin/$(basename "$bin")"
done

"$PREFIX/bin/firecracker" --version | head -1
"$PREFIX/bin/vmlayerd" --version
ls -la /dev/kvm
echo "provisioned in $PREFIX"
