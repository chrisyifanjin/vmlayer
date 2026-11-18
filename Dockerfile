# syntax=docker/dockerfile:1
# vmlayerd with the Firecracker backend: the daemon, the CLI, the per-VM
# shim, the static guest agent, Firecracker + jailer and a guest kernel.
#
#   docker build -t vmlayerd .
#   docker run --privileged --network host --pid host \
#     -v /var/lib/vmlayer:/var/lib/vmlayer -v /run/vmlayer:/run/vmlayer vmlayerd
#
# Kubernetes: deploy/kubernetes/. Without /dev/kvm the daemon serves the
# Docker backend if /var/run/docker.sock is mounted.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add "$(uname -m)-unknown-linux-musl"
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p vmlayerd -p vmlayer-cli -p vmlayer-backend-firecracker \
    && cargo build --release --locked -p vmlayer-guest-agent --target "$(uname -m)-unknown-linux-musl" \
    && mkdir /out \
    && cp target/release/vmlayerd target/release/vmlayer target/release/vmlayer-shim \
          "target/$(uname -m)-unknown-linux-musl/release/vmlayer-guest-agent" /out/

FROM debian:bookworm-slim
ARG FIRECRACKER_VERSION=v1.18.0
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl iproute2 nftables iptables e2fsprogs kmod \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-firecracker.sh /usr/local/lib/vmlayer/fetch-firecracker.sh
RUN PREFIX=/opt/vmlayer FIRECRACKER_VERSION="$FIRECRACKER_VERSION" \
    bash /usr/local/lib/vmlayer/fetch-firecracker.sh
COPY --from=build /out/ /opt/vmlayer/bin/
COPY deploy/container/vmlayerd.toml /etc/vmlayer/vmlayerd.toml
ENV PATH=/opt/vmlayer/bin:$PATH \
    VMLAYER_CONFIG=/etc/vmlayer/vmlayerd.toml \
    VMLAYER_ENDPOINT=unix:///run/vmlayer/vmlayerd.sock
VOLUME ["/var/lib/vmlayer"]
ENTRYPOINT ["vmlayerd"]
