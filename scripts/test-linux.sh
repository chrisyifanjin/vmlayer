#!/usr/bin/env sh
# Run the Rust tests that need Linux tools (mkfs.ext4, debugfs) inside a
# container, as root, from any host with Docker.
# Usage: scripts/test-linux.sh [cargo test args]   (default: -p vmlayerd images)
set -eu
cd "$(dirname "$0")/.."
docker run --rm \
  -v "$PWD:/src:ro" \
  -v vmlayer-cargo-registry:/usr/local/cargo/registry \
  -v vmlayer-target-linux-test:/build \
  -e VMLAYER_NET_TESTS=1 -e CARGO_TARGET_DIR=/build \
  -w /src \
  rust:1-alpine sh -c "
    apk add --no-cache musl-dev protobuf-dev e2fsprogs e2fsprogs-extra >/dev/null
    cargo test ${*:--p vmlayerd images} >/tmp/out 2>&1; code=\$?
    grep -E '^test |test result|panicked|^error' /tmp/out
    exit \$code"
