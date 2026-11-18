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
| `--token` | `VMLAYER_TOKEN` | an administrator token; tcp needs it or `[auth] tokens` |
| `--tls-cert`, `--tls-key`, `--tls-client-ca` | `VMLAYER_TLS_CERT`, `VMLAYER_TLS_KEY`, `VMLAYER_TLS_CLIENT_CA` | plaintext tcp |
| `--data-dir` | `VMLAYER_DATA_DIR` | `~/.vmlayer` or `/var/lib/vmlayer` |
| `--config file.toml` | `VMLAYER_CONFIG` | none |
| `--log-format pretty\|json`, `--log-filter` | `VMLAYER_LOG_FORMAT`, `VMLAYER_LOG` | pretty, info |
| `--default-backend` | `VMLAYER_DEFAULT_BACKEND` | strongest available |
| `--blob-store s3://bucket/prefix` | `VMLAYER_BLOB_STORE` | none (durable snapshots off) |
| `--advertise tcp://10.0.0.5:7070`, `--host-id` | `VMLAYER_ADVERTISE`, `VMLAYER_HOST_ID` | not in the host registry; hostname |
| `--heartbeat-secs`, `--lease-ttl-secs` | `VMLAYER_HEARTBEAT_SECS`, `VMLAYER_LEASE_TTL_SECS` | 10, 30 |
| `--rescue`, `--checkpoint-secs` | `VMLAYER_RESCUE`, `VMLAYER_CHECKPOINT_SECS` | off, 60 |

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
pids_limit = 4096                      # max processes per sandbox; 0 disables

[blob_store]                           # durable volume snapshots (decision 0011)
url = "s3://my-bucket/vmlayer"         # or az://container/prefix, gs://bucket/prefix, file:///mnt/shared
options = { region = "eu-central-1" }  # optional; e.g. endpoint, allow_http, use_emulator
gc_grace_hours = 24                    # unreferenced chunks younger than this survive GC
concurrency = 16                       # requests in flight per transfer
lazy_restore = true                    # block volumes usable at once, fetched on demand (needs `modprobe nbd`)

[cluster]                              # several hosts sharing the blob store (decision 0014)
advertise = "tcp://10.0.0.5:7070"      # publish this host in the registry
host_id = "host-a"                     # default: the hostname
labels = { zone = "a" }                # for placement constraints
heartbeat_secs = 10
lease_ttl_secs = 30
rescue = true                          # recreate rescheduled sandboxes of dead hosts here
checkpoint_secs = 60                   # workspace checkpoint interval for rescheduled sandboxes

[admission]                            # refuse what the host cannot hold (review A3)
enabled = true
memory_overcommit = 1.0                # allocatable memory = (host memory - reserve) x this
vcpu_overcommit = 4.0                  # allocatable vCPUs = host CPUs x this
reserved_memory_mib = 512

[log]
format = "json"
filter = "info,vmlayerd=debug"
```

### Blob store credentials

The daemon never reads credentials from its config file. It uses the
standard sources of each provider:

| Store | URL | Credentials |
|---|---|---|
| AWS S3 | `s3://bucket/prefix` | `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, `AWS_PROFILE`-style web identity (IRSA), or instance metadata |
| S3-compatible (MinIO, Ceph, R2) | `s3://bucket/prefix` | as above, plus `options = { endpoint = "https://..." }` |
| Azure Blob | `az://container/prefix` | `AZURE_STORAGE_ACCOUNT_NAME` with `AZURE_STORAGE_ACCOUNT_KEY`, a SAS token, workload identity, or managed identity |
| Google Cloud Storage | `gs://bucket/prefix` | `GOOGLE_APPLICATION_CREDENTIALS` or the VM's service account |
| Shared directory | `file:///mnt/shared/vmlayer` | file permissions |

Give the daemon list, read, write and delete on its prefix. Run garbage
collection (`vmlayer volume gc`) from one place on a schedule, for example
daily; it is safe to run while snapshots are being uploaded.

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

Callers on the unix socket are the operator. TCP callers need a token,
and should use TLS (decision 0020):

```toml
listen = "tcp://0.0.0.0:7443"

[tls]
cert = "/etc/vmlayer/tls/server.pem"
key = "/etc/vmlayer/tls/server.key"
client_ca = "/etc/vmlayer/tls/clients-ca.pem"   # optional: require client certificates

# One entry per token, by its SHA-256 (printf %s "$TOKEN" | sha256sum).
[[auth.tokens]]
name = "agents-prod"
sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
tenant = "acme"          # unset: every tenant
role = "write"           # read | write | admin

[[auth.tokens]]
name = "dashboard"
sha256 = "..."
role = "read"
```

What each part does:

- **TLS** encrypts the connection. With `client_ca`, a caller must also
  present a client certificate signed by that CA (mutual TLS).
- **The token** decides the caller's role and tenant. Mutual TLS does not
  replace it.
- **Roles.** `read` lists and reads. `write` also creates, changes,
  deletes and runs commands. `admin` also runs host-wide operations
  (blob store garbage collection).
- **Tenants.** A tenant token sees only its tenant's sandboxes, volumes
  and snapshots. Another tenant's resources answer "not found". Volume
  names are per tenant.

`--token` adds one administrator token with no tenant, the quick way to
get started:

```bash
vmlayerd --listen tcp://127.0.0.1:7443 --token "$(openssl rand -hex 32)"
```

The daemon warns when it serves plaintext on a non-loopback address. In
the host registry, advertise `tls://host:port` so cluster clients use TLS
too.

Clients:

```bash
vmlayer --endpoint tls://10.0.0.5:7443 --token "$TOKEN" \
  --tls-ca ca.pem --tls-cert client.pem --tls-key client.key info
```

## Port gateway

Sandboxes can publish ports at `https://<port>-<sandbox-id>.<domain>`,
each with its own token (decision 0021). Point a wildcard DNS record at
the daemon and give it a wildcard certificate:

```toml
[gateway]
listen = "0.0.0.0:443"
domain = "sandboxes.example.com"     # DNS: *.sandboxes.example.com -> this host
cert = "/etc/vmlayer/tls/wildcard.pem"
key = "/etc/vmlayer/tls/wildcard.key"
# public_port = 443                  # behind a load balancer on another port
```

Without `cert` and `key` the gateway speaks plain HTTP, for use behind a
TLS-terminating load balancer that forwards `Host` unchanged. Ports are
published only on request (`expose(port, gateway=True)`), and the gateway
stores only a hash of each token.

## Credential proxy

Give sandboxes API access without giving them keys (decision 0022). The
daemon reads each secret from its own environment or a file:

```toml
[credential_proxy]
listen = "0.0.0.0:7072"       # sandboxes reach it at vmlayer.host:7072; firewall it from outside

[[credential_proxy.credentials]]
name = "anthropic"
upstream = "https://api.anthropic.com"
header = "x-api-key"
secret_env = "ANTHROPIC_API_KEY"          # or secret_file = "/run/secrets/anthropic"
guest_env = { ANTHROPIC_BASE_URL = "{url}", ANTHROPIC_API_KEY = "{placeholder}" }

[[credential_proxy.credentials]]
name = "openai"
upstream = "https://api.openai.com"
header = "authorization"
value = "Bearer {secret}"
secret_env = "OPENAI_API_KEY"
guest_env = { OPENAI_BASE_URL = "{url}/v1", OPENAI_API_KEY = "{placeholder}" }
tenants = ["acme"]                        # optional: only these tenants
```

A sandbox created with `credentials=["anthropic"]` gets a placeholder and
the base URL. The proxy replaces the placeholder with the secret on the
way out. On Firecracker the host opens only this port to guests, so the
rest of the host stays closed. The daemon refuses to start if a secret
cannot be read.

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
