# Firecracker backend: host setup

Status: **working on real KVM** (Firecracker 1.18, aarch64, nested
virtualization): boot, exec, files, pty, networking, volumes, memory
snapshots, restore, fork. Bare-metal x86_64 has not been run yet.

The quickest way to try it on a laptop is `scripts/kvm-vm/up.sh`, which
builds everything described below inside a small VM with KVM
([details](../scripts/kvm-vm/README.md)).

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
max_concurrent_boots = 0      # cold boots at once; 0 = one per host CPU
boot_timeout_secs = 30        # for the guest agent to come up after boot
template_snapshots = false    # restore volume-less creates from a boot template
template_limit = 4            # boot templates kept on disk, most recently used first
```

At startup the daemon installs host rules (`sudo nft list table ip
vmlayer`): guests cannot open connections to the host itself or to each
other, only to the outside world through NAT. The resolvers in `dns` are
also the upstreams of the per-VM DNS proxy used for egress allowlists.

A guest kernel boot is CPU-bound. When more creates arrive than
`max_concurrent_boots`, the extra ones wait for a slot instead of starting
together and slowing each other past the boot timeout; the wait does not
count against `boot_timeout_secs`. Restores from a snapshot or a boot
template are not limited.

`vmlayer info` shows the backend as `ready` or the exact reason it is not
(no `/dev/kvm`, missing binary, missing kernel).

## Root images

Any OCI image reference works, exactly as with Docker:

```python
Sandbox.create("python:3.12", backend=Backend.FIRECRACKER)
```

The first time an image is used the daemon pulls its layers itself (no
Docker engine needed; credentials come from `~/.docker/config.json`), applies
them with OCI whiteout rules, and builds one ext4 image with `mkfs.ext4 -d`,
cached under `<data>/images/<digest>/rootfs.ext4`. Every sandbox then boots
from a copy-on-write clone of that file. Pre-pull to keep the first create
fast:

```bash
vmlayer pull python:3.12 --backend firecracker
```

An absolute path to an existing ext4 file is also accepted and used as is,
for hand-built images.

Either way the daemon caches a prepared ext4 image with the guest agent at
`/vmlayer-guest-agent`, then clones that image for each sandbox. The kernel
runs the agent as `init`. The image needs
nothing else: no init system, no network scripts. The image's
`ENTRYPOINT`/`CMD`, `ENV` and `WORKDIR` are honoured like on Docker. See
[decision 0009](decisions/0009-image-pipeline.md).

## What happens on `create`

1. Jail: `<data>/firecracker/jail/firecracker/<id>/root/` with `vmlinux`,
   `root.ext4` (clone), `vol0.ext4`... (hard links to the volumes).
2. Namespace `vml-N`: `tap0` (172.16.0.1/30), veth to the host on a /30,
   NAT out, DNAT in, cloud metadata dropped, and with an egress policy the
   allowlist firewall plus a DNS proxy on the host end of the veth
   (decision 0012). Inspect with `sudo ip netns exec vml-N nft list ruleset`.
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
