#!/usr/bin/env bash
# Installs Firecracker + jailer (official release) and a guest kernel
# (Firecracker's CI bucket) under $PREFIX/bin and $PREFIX/kernel.
# Used by scripts/kvm-vm/provision.sh and the container image (Dockerfile).
#
#   PREFIX=/opt/vmlayer FIRECRACKER_VERSION=v1.18.0 scripts/fetch-firecracker.sh
set -euo pipefail
PREFIX="${PREFIX:-/opt/vmlayer}"
ARCH="$(uname -m)"
mkdir -p "$PREFIX/bin" "$PREFIX/kernel"

if [ ! -x "$PREFIX/bin/firecracker" ]; then
  rel="https://github.com/firecracker-microvm/firecracker/releases"
  ver="${FIRECRACKER_VERSION:-$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$rel/latest")")}"
  echo "firecracker $ver"
  tmp="$(mktemp -d)"
  curl -fsSL "$rel/download/$ver/firecracker-$ver-$ARCH.tgz" | tar -xz -C "$tmp"
  install -m 0755 "$tmp/release-$ver-$ARCH/firecracker-$ver-$ARCH" "$PREFIX/bin/firecracker"
  install -m 0755 "$tmp/release-$ver-$ARCH/jailer-$ver-$ARCH" "$PREFIX/bin/jailer"
  echo "$ver" > "$PREFIX/firecracker.version"
  rm -rf "$tmp"
fi

# Kernels are published per release line and not for every line, so take
# the newest line that has one (or GUEST_KERNEL_KEY to pin it).
if [ ! -f "$PREFIX/kernel/vmlinux" ]; then
  bucket="http://spec.ccfc.min.s3.amazonaws.com"
  key="${GUEST_KERNEL_KEY:-}"
  if [ -z "$key" ]; then
    for line in $(curl -fsSL "$bucket/?prefix=firecracker-ci/&delimiter=/&list-type=2" \
        | grep -oE 'firecracker-ci/v[0-9]+\.[0-9]+/' | sort -rV); do
      key="$(curl -fsSL "$bucket/?prefix=${line}$ARCH/vmlinux-&list-type=2" \
        | grep -oE "${line}$ARCH/vmlinux-[0-9]+\.[0-9]+\.[0-9]+" | sort -V | tail -1 || true)"
      [ -n "$key" ] && break
    done
  fi
  [ -n "$key" ] || { echo "no guest kernel found in $bucket" >&2; exit 1; }
  echo "kernel $key"
  curl -fsSL "https://s3.amazonaws.com/spec.ccfc.min/$key" -o "$PREFIX/kernel/vmlinux"
  echo "$key" > "$PREFIX/kernel/source"
fi
