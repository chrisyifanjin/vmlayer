#!/usr/bin/env sh
# Build a static Linux vmlayer-guest-agent inside Docker (works on macOS too).
# Output: target/guest-agent/<arch>/vmlayer-guest-agent
# Set GUEST_ARCH=x86_64 or aarch64 (default: the Docker engine's own arch).
set -eu
cd "$(dirname "$0")/.."
arch="${GUEST_ARCH:-$(docker version --format '{{.Server.Arch}}' | sed 's/arm64/aarch64/;s/amd64/x86_64/')}"
platform="linux/$(echo "$arch" | sed 's/aarch64/arm64/;s/x86_64/amd64/')"
out="target/guest-agent/$arch"
mkdir -p "$out"
docker run --rm --platform "$platform" \
  -v "$PWD:/src:ro" \
  -v vmlayer-cargo-registry:/usr/local/cargo/registry \
  -v "vmlayer-target-$arch:/build" \
  -w /src \
  rust:1-alpine sh -c "
    apk add --no-cache musl-dev protobuf-dev >/dev/null &&
    CARGO_TARGET_DIR=/build cargo build --release -p vmlayer-guest-agent --target $arch-unknown-linux-musl 2>&1 | tail -3 &&
    cp /build/$arch-unknown-linux-musl/release/vmlayer-guest-agent /src/$out/ 2>/dev/null || cat /build/$arch-unknown-linux-musl/release/vmlayer-guest-agent > /dev/stdout" > "$out/vmlayer-guest-agent.tmp" || true
# The source mount is read-only; the binary came through stdout.
if [ -s "$out/vmlayer-guest-agent.tmp" ] && head -c4 "$out/vmlayer-guest-agent.tmp" | grep -q ELF; then
  mv "$out/vmlayer-guest-agent.tmp" "$out/vmlayer-guest-agent" && chmod +x "$out/vmlayer-guest-agent"
  echo "built $out/vmlayer-guest-agent ($(du -h "$out/vmlayer-guest-agent" | cut -f1))"
else
  echo "build failed:"; cat "$out/vmlayer-guest-agent.tmp"; rm -f "$out/vmlayer-guest-agent.tmp"; exit 1
fi
