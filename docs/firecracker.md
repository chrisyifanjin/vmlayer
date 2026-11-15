# Firecracker backend: host setup

Status: implemented and unit-tested, **not yet run on a KVM host**. This page
is the setup the first run will use; expect it to change.

## Requirements

- Linux with `/dev/kvm` (bare metal, or a cloud VM with nested virtualization;
  see [decision 0005](decisions/0005-cloud-agnostic.md) for which clouds).
- `firecracker` and `jailer` 1.16+ from the
  [releases](https://github.com/firecracker-microvm/firecracker/releases).
- An uncompressed guest kernel (`vmlinux`) built with Firecracker's guest
  config; the Firecracker CI artifacts work for a first test.
- `iproute2`, `nftables`, `e2fsprogs` (`mkfs.ext4`, `debugfs`).
- The static guest agent: `scripts/build-guest-agent.sh` (via Docker) or
  `cargo build --release -p vmlayer-guest-agent --target x86_64-unknown-linux-musl`.
- `vmlayerd` running as root (taps, network namespaces, chroots).

## Configuration

```toml
[firecracker]
enabled = true
firecracker_bin = "/usr/local/bin/firecracker"
jailer_bin = "/usr/local/bin/jailer"
kernel = "/var/lib/vmlayer/vmlinux"
guest_agent = "/usr/local/bin/vmlayer-guest-agent"
jail_uid = 65534
jail_gid = 65534
host_subnet = "10.200.0.0/16"
dns = ["1.1.1.1", "8.8.8.8"]
```

`vmlayer info` shows the backend as `ready` or the exact reason it is not
(no `/dev/kvm`, missing binary, missing kernel).

## Root images

Until the OCI pipeline lands (roadmap step 3) the image reference passed to
`Sandbox.create` is an absolute path to an ext4 root image on the host:

```bash
# A minimal Alpine root image, 1 GiB, with sh/coreutils and python:
truncate -s 1G /var/lib/vmlayer/images/alpine.ext4
mkfs.ext4 -q -F /var/lib/vmlayer/images/alpine.ext4
mkdir -p /mnt/img && mount /var/lib/vmlayer/images/alpine.ext4 /mnt/img
docker export "$(docker create python:3.12-alpine)" | tar -C /mnt/img -x
umount /mnt/img
```

```python
Sandbox.create("/var/lib/vmlayer/images/alpine.ext4", backend=Backend.FIRECRACKER)
```

The daemon clones the image per sandbox (copy-on-write on XFS/btrfs) and
injects the guest agent as `/vmlayer-guest-agent`, which the kernel runs as
`init`. The image needs nothing else: no init system, no network scripts.

## What happens on `create`

1. Jail: `<data>/firecracker/jail/firecracker/<id>/root/` with `vmlinux`,
   `root.ext4` (clone), `vol0.ext4`... (hard links to the volumes).
2. Namespace `vml-N`: `tap0` (172.16.0.1/30), veth to the host on a /30,
   NAT out, DNAT in.
3. `jailer --netns ... -- --api-sock /api.sock`, then the API calls:
   machine config, boot source with `ip=172.16.0.2::172.16.0.1:... init=/vmlayer-guest-agent`,
   drives, `eth0` on `tap0`, vsock, `InstanceStart`.
4. Connect to the agent over `/v.sock` (`CONNECT 5200`), wait for `Ready`,
   mount volumes at their guest paths, start the entrypoint.

Snapshot pauses the VM, writes `vmstate` and `mem` into
`<data>/firecracker/snapshots/<id>/`, clones the root disk beside them and
resumes. Restore and fork populate a new jail from that directory and load
the snapshot with the new namespace's `tap0`.

## Debugging the first boot

- `VMLAYER_LOG=debug,vmlayer_backend_firecracker=trace vmlayerd`
- The jail keeps `api.sock`; `curl --unix-socket .../api.sock http://localhost/`
  shows the VMM state.
- Serial console is `ttyS0`; add `console=ttyS0` output capture by running
  Firecracker by hand with the same arguments (the jail directory has
  everything it needs).
- The agent logs to its stderr, i.e. the serial console, at
  `VMLAYER_AGENT_LOG` (set it via `kernel_args`).
