#!/usr/bin/env bash
# Runs INSIDE the KVM VM: (re)start vmlayerd as root with the Firecracker
# backend, using what provision.sh installed under $PREFIX.
set -euo pipefail
PREFIX="${PREFIX:-/opt/vmlayer}"
cat > "$PREFIX/vmlayerd.toml" <<TOML
data_dir = "/var/lib/vmlayer"
listen = "unix:///run/vmlayer/vmlayerd.sock"
default_backend = "firecracker"

[docker]
enabled = false

[firecracker]
enabled = true
firecracker_bin = "$PREFIX/bin/firecracker"
jailer_bin = "$PREFIX/bin/jailer"
kernel = "$PREFIX/kernel/vmlinux"
guest_agent = "$PREFIX/bin/vmlayer-guest-agent"
template_snapshots = true
template_warmup_argv = ["python3", "-c", "pass"]
prefetch_memory = ${VMLAYER_PREFETCH_MEMORY:-true}

# Durable volume snapshots. A local directory here; point it at
# s3://, az:// or gs:// to share snapshots between hosts.
[blob_store]
url = "${VMLAYER_BLOB_STORE:-file:///var/lib/vmlayer-blobs}"
lazy_restore = ${VMLAYER_LAZY_RESTORE:-true}

[cluster]
lease_ttl_secs = ${VMLAYER_LEASE_TTL_SECS:-30}

${VMLAYER_EXTRA_CONFIG:-}
TOML
# Lazy volume restore serves volumes through NBD devices (decision 0016).
sudo modprobe nbd nbds_max=64 2>/dev/null || true
sudo pkill -x vmlayerd 2>/dev/null || true
sleep 0.5
sudo mkdir -p /run/vmlayer /var/lib/vmlayer /var/lib/vmlayer-blobs
sudo sh -c "VMLAYER_LOG='${VMLAYER_LOG:-info,vmlayerd=debug,vmlayer_backend_firecracker=debug}' nohup $PREFIX/bin/vmlayerd --config $PREFIX/vmlayerd.toml > /var/log/vmlayerd.log 2>&1 &"
for _ in $(seq 1 50); do
  sudo VMLAYER_ENDPOINT=unix:///run/vmlayer/vmlayerd.sock "$PREFIX/bin/vmlayer" info >/dev/null 2>&1 && break
  sleep 0.2
done
# Development VM only: let the unprivileged SSH user reach the socket so it
# can be forwarded to the host (see scripts/kvm-vm/README.md).
sudo chmod 0666 /run/vmlayer/vmlayerd.sock
sudo VMLAYER_ENDPOINT=unix:///run/vmlayer/vmlayerd.sock "$PREFIX/bin/vmlayer" info
