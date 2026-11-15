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
. "$HOME/.cargo/env"
rustup target add "$ARCH-unknown-linux-musl" >/dev/null

sudo mkdir -p "$PREFIX/bin" "$PREFIX/kernel"
sudo chown -R "$(id -u):$(id -g)" "$PREFIX"

# Firecracker + jailer from the official release.
if [ ! -x "$PREFIX/bin/firecracker" ]; then
  rel="https://github.com/firecracker-microvm/firecracker/releases"
  ver="${FIRECRACKER_VERSION:-$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$rel/latest")")}"
  echo "firecracker $ver"
  curl -fsSL "$rel/download/$ver/firecracker-$ver-$ARCH.tgz" | tar -xz -C /tmp
  cp "/tmp/release-$ver-$ARCH/firecracker-$ver-$ARCH" "$PREFIX/bin/firecracker"
  cp "/tmp/release-$ver-$ARCH/jailer-$ver-$ARCH" "$PREFIX/bin/jailer"
  echo "$ver" > "$PREFIX/firecracker.version"
fi

# Guest kernel from Firecracker's own CI bucket. Kernels are published per
# release line and not for every line, so take the newest line that has one.
if [ ! -f "$PREFIX/kernel/vmlinux" ]; then
  bucket="http://spec.ccfc.min.s3.amazonaws.com"
  key=""
  for line in $(curl -fsSL "$bucket/?prefix=firecracker-ci/&delimiter=/&list-type=2" \
      | grep -oE 'firecracker-ci/v[0-9]+\.[0-9]+/' | sort -rV); do
    key="$(curl -fsSL "$bucket/?prefix=${line}$ARCH/vmlinux-&list-type=2" \
      | grep -oE "${line}$ARCH/vmlinux-[0-9]+\.[0-9]+\.[0-9]+" | sort -V | tail -1 || true)"
    [ -n "$key" ] && break
  done
  [ -n "$key" ] || { echo "no guest kernel found in $bucket" >&2; exit 1; }
  echo "kernel $key"
  curl -fsSL "https://s3.amazonaws.com/spec.ccfc.min/$key" -o "$PREFIX/kernel/vmlinux"
fi

export CARGO_TARGET_DIR="$HOME/target"
cd "$REPO"
cargo build --release --locked -p vmlayerd -p vmlayer-cli 2>&1 | tail -2
cargo build --release --locked -p vmlayer-guest-agent --target "$ARCH-unknown-linux-musl" 2>&1 | tail -1
# Replace atomically: a running vmlayerd keeps its old inode ("text file busy"
# would otherwise fail the copy and silently leave the old binary in place).
for bin in "$CARGO_TARGET_DIR/release/vmlayerd" "$CARGO_TARGET_DIR/release/vmlayer" \
           "$CARGO_TARGET_DIR/$ARCH-unknown-linux-musl/release/vmlayer-guest-agent"; do
  install -m 0755 "$bin" "$PREFIX/bin/.$(basename "$bin").new"
  mv -f "$PREFIX/bin/.$(basename "$bin").new" "$PREFIX/bin/$(basename "$bin")"
done

"$PREFIX/bin/firecracker" --version | head -1
"$PREFIX/bin/vmlayerd" --version
ls -la /dev/kvm
echo "provisioned in $PREFIX"
