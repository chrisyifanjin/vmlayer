# Deploying vmlayerd

## Build

```bash
cargo build --release
# target/release/vmlayerd   the daemon
# target/release/vmlayer    the CLI
```

`protoc` is needed at build time (`apt install protobuf-compiler`,
`brew install protobuf`). Release builds for Linux x86_64/aarch64 and macOS
arm64 are published on tags; see `.github/workflows/release.yml`.

## Run

Defaults work without a config file:

```bash
vmlayerd                       # ~/.vmlayer, unix socket ~/.vmlayer/vmlayerd.sock
sudo vmlayerd                  # /var/lib/vmlayer, socket under $XDG_RUNTIME_DIR or the data dir
```

Flags and environment:

| Flag | Env | Default |
|---|---|---|
| `--listen unix:///path` or `tcp://host:port` | `VMLAYER_LISTEN` | unix socket |
| `--token` | `VMLAYER_TOKEN` | required for tcp |
| `--data-dir` | `VMLAYER_DATA_DIR` | `~/.vmlayer` or `/var/lib/vmlayer` |
| `--config file.toml` | `VMLAYER_CONFIG` | none |
| `--log-format pretty\|json`, `--log-filter` | `VMLAYER_LOG_FORMAT`, `VMLAYER_LOG` | pretty, info |
| `--default-backend` | `VMLAYER_DEFAULT_BACKEND` | strongest available |

Config file (every key optional):

```toml
listen = "unix:///run/vmlayer/vmlayerd.sock"
data_dir = "/var/lib/vmlayer"
default_backend = "docker"

[defaults]
vcpus = 1
memory_mib = 512
volume_size_mib = 8192
workdir = "/work"
output_buffer_bytes = 8388608   # per-process output kept for re-attach

[docker]
enabled = true
host = "unix:///var/run/docker.sock"   # or DOCKER_HOST, or auto-discovered
network = "bridge"
socat_image = "alpine/socat:1.8.0.0"
probe_image = "alpine:3.20"
verify_mounts = true                   # check the data dir is visible in containers

[log]
format = "json"
filter = "info,vmlayerd=debug"
```

## systemd

```ini
# /etc/systemd/system/vmlayerd.service
[Unit]
Description=vmlayer host daemon
After=network.target docker.service
Wants=docker.service

[Service]
ExecStart=/usr/local/bin/vmlayerd --config /etc/vmlayer/vmlayerd.toml
Restart=on-failure
RestartSec=2
# The daemon re-attaches to sandboxes that are still running after a restart.
Environment=VMLAYER_LOG_FORMAT=json

[Install]
WantedBy=multi-user.target
```

```bash
sudo install -m 0755 target/release/vmlayerd target/release/vmlayer /usr/local/bin/
sudo mkdir -p /etc/vmlayer /var/lib/vmlayer
sudo systemctl enable --now vmlayerd
vmlayer info
```

The socket is created with mode 0660. Put SDK users in the daemon's group or
run it under a dedicated user they share.

## Remote access

```bash
vmlayerd --listen tcp://0.0.0.0:7443 --token "$(openssl rand -hex 32)"
```

The daemon refuses TCP without a token. Put TLS in front of it (a reverse
proxy or an overlay network); the daemon itself speaks plain HTTP/2.

## Data directory

```
<data_dir>/
  state.db             SQLite, WAL mode
  volumes/<id>/        volume contents (Docker backend: bind-mounted directories)
  volume-snapshots/<id>/
  vmlayerd.sock        default socket
```

Clone and snapshot are copy-on-write on XFS or btrfs (`cp --reflink`) and
APFS (`cp -c`); plain copies elsewhere. Put the data dir on XFS or btrfs on
Linux hosts that take many snapshots.

On macOS with VM-based engines (Docker Desktop, colima, OrbStack) the data
dir must be inside a path shared with the engine, usually your home
directory. The daemon verifies this at startup and reports the reason in
`vmlayer info` if not.

## Upgrades

Stop the daemon, replace the binary, start it. Running sandboxes survive:
the daemon re-attaches to them from `state.db`. Snapshots record the engine
version that made them; a memory snapshot from one Firecracker release does
not restore on another.
