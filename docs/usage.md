# Using vmlayer

This is the Python SDK guide. The TypeScript SDK (`sdk/typescript`) has the
same methods in camelCase and returns promises; `await using sb = await
Sandbox.create(...)` kills the sandbox at the end of the block. The CLI
mirrors both (`vmlayer --help`).

## Connecting

The SDK talks to `vmlayerd` over its unix socket. Nothing to configure when the
daemon runs on the same machine with defaults; otherwise:

```python
import vmlayer

client = vmlayer.Client("unix:///run/vmlayer/vmlayerd.sock")
client = vmlayer.Client("tcp://10.0.0.5:7443", token="...")   # remote daemon
vmlayer.set_default_client(client)                              # used by Sandbox.create etc.

client.info()                 # daemon version, backends, engine versions
client.available_backends()   # {Backend.DOCKER: "29.5.2"}
```

`VMLAYER_ENDPOINT` sets the default endpoint without code changes.

Pulling an image ahead of time makes the first `create` fast (on microVM
backends it also builds the root filesystem):

```python
client.pull_image("python:3.12")          # or: vmlayer pull python:3.12
```

## Sandboxes

```python
from datetime import timedelta
from vmlayer import Sandbox, Backend

sb = Sandbox.create(
    "python:3.12",
    cpus=2, memory_mb=1024,
    env={"PYTHONUNBUFFERED": "1"},
    timeout=timedelta(minutes=30),   # hard lifetime; the daemon kills it after
    backend=Backend.AUTO,            # strongest available; or DOCKER / FIRECRACKER
    labels={"task": "swe-42"},
)
sb.id, sb.state, sb.capabilities
```

Use it as a context manager so it is always killed:

```python
with Sandbox.create("alpine:3.20") as sb:
    ...
```

### Commands

```python
r = sb.exec("pytest -q", cwd="/work/repo", timeout=timedelta(minutes=10))
r.exit_code, r.ok, r.stdout, r.stderr
r.output          # stdout and stderr interleaved in arrival order: what you show the model
```

A string runs through `/bin/sh -c`; a list is exec'd directly. A non-zero
exit is not an exception. A timeout is: `ExecTimeout` carries `.partial`
with whatever was produced before the kill.

Streaming and interactive:

```python
p = sb.spawn("python train.py")          # returns immediately
for chunk in p:                           # OutputChunk(stream, data)
    print(chunk.data.decode(), end="")
result = p.wait()

p = sb.spawn("python -i")
p.stdin.write(b"print(2+2)\n"); p.stdin.close()

sb.process(p.pid)                        # re-attach after a reconnect; output is buffered
sb.processes()                           # everything vmlayer started, incl. the entrypoint
```

A real terminal, for TUIs or xterm.js:

```python
with sb.pty("bash", size=vmlayer.PtySize(cols=120, rows=40)) as t:
    t.write(b"ls --color\n")
    for data in t:
        ...
    t.resize(vmlayer.PtySize(cols=80, rows=24))
```

### Files

```python
sb.files.write("/work/main.py", "print(1)")       # parents created
sb.files.read("/work/out.bin")
sb.files.read_text("/work/log.txt")
sb.files.list("/work")                             # [FileInfo(path, size, is_dir, mode, modified_at)]
sb.files.stat("/work/main.py"); sb.files.exists("/work/x")
sb.files.mkdir("/work/data"); sb.files.remove("/work/tmp", recursive=True)
sb.files.upload("./repo", "/work/repo")           # whole tree
sb.files.download("/work/dist", "./dist")
```

### Server-shaped images

Many images are a server (a Jupyter kernel gateway, an agent server, a dev
server). The image's `ENTRYPOINT`/`CMD` starts automatically as process 1;
override with `entrypoint=[...]` or disable with `entrypoint=None`.

```python
sb = Sandbox.create("jupyter/kernel-gateway")
sb.wait_for_port(8888, timeout=timedelta(seconds=30))
url = sb.expose(8888).url          # http://127.0.0.1:PORT on the host
sb.process(1).wait()               # the entrypoint's output
```

### Pause, snapshot, restore

```python
sb.pause(); sb.resume()                 # stop the clock, keep resources

snap = sb.snapshot(labels={"stage": "deps-installed"})
# Docker: filesystem of the root disk + every volume. Firecracker: + RAM.
sb2 = Sandbox.restore(snap)             # new id, new clones of every volume
snap.delete()
client.list_snapshots(labels={"stage": "deps-installed"})
```

`fork(n)` is the same idea for running sandboxes (microVM backends only).

### Network policy and resource limits

On the Firecracker backend every sandbox is cut off from the host, from
other sandboxes and from cloud metadata services (`169.254.169.254`).
Outbound traffic is open unless you give an allowlist:

```python
sb = Sandbox.create(
    "python:3.12",
    egress=["pypi.org", "*.pythonhosted.org:443", "github.com:22,443", "10.0.0.0/8:5432"],
    limits=vmlayer.IoLimits(network_mbps=100, disk_mbps=200, disk_iops=5000),
)
sb.exec("pip install requests")        # allowed
sb.exec("curl https://example.com")    # fails at once: name does not resolve
```

Rules are IPv4 addresses or networks, exact DNS names, or `*.domain` for
every name under a domain, each with optional ports. `egress=[]` blocks all
outbound traffic; `network=False` removes the network device entirely.
DNS from the sandbox goes through a resolver that answers only allowed
names, whichever server the guest asks, and opens each answer's addresses
for the record's lifetime. The Docker backend cannot enforce `egress` or
`limits` and raises `BackendUnsupported`; check
`sandbox.capabilities.egress_policy` and `io_limits`. Details are in
[decision 0012](decisions/0012-network-isolation-egress-and-io-limits.md).

### Several hosts

Daemons that share a blob store and are started with `--advertise` form a
cluster with no extra service. Place sandboxes through any one of them:

```python
cluster = vmlayer.Cluster("tcp://10.0.0.5:7070", token=TOKEN)
[h.host_id for h in cluster.hosts()]           # fresh heartbeats
sb = cluster.create("python:3.12", memory_mb=2048, placement={"zone": "a"})
cluster.connect(sb.id)                         # find it again from anywhere

# At most one live sandbox named "repo-42" across all hosts:
Sandbox.create("python:3.12", lease="repo-42", client=cluster.client(cluster.place()))
```

A host that loses a lease, or cannot renew it before it expires, kills the
sandbox holding it, so a partitioned host cannot keep a second copy running.

Add `reschedule=True` to survive host failures. The daemon checkpoints the
workspace to the blob store every `checkpoint_secs`, and any daemon started
with `--rescue` recreates the sandbox from the last checkpoint once the
dead host's lease expires. It boots fresh: memory state and writes after
the last checkpoint are lost
([decision 0015](decisions/0015-rescheduling.md)).
Leases need an object store with conditional writes (S3, Azure Blob, GCS).
Design: [decision 0014](decisions/0014-hosts-placement-and-leases.md).

## Volumes: workspaces that outlive sandboxes

Every sandbox gets an anonymous volume at its `workdir` (`/work` by default)
that dies with it. Name one and it persists:

```python
from vmlayer import Volume

repo = Volume.create(name="repo-42", size_mb=8192, labels={"repo": "org/x"})
with Sandbox.create("python:3.12", workspace=repo) as sb:
    sb.exec("git clone https://github.com/org/x .")
    sb.exec("pip install -e .")

ckpt = repo.snapshot()                                  # milliseconds, copy-on-write
with Sandbox.create("python:3.12", workspace=repo) as sb:
    sb.exec("pytest")                                   # same files, fresh sandbox

fresh = Volume.from_snapshot(ckpt, name="repo-42-try2") # a template, instantiated
sb = Sandbox.create("python:3.12", workspace=fresh)
```

Rules:

- A volume is attached read-write to one sandbox at a time; a second attach
  raises `VolumeBusy`. Read-only attachments are unlimited:
  `Sandbox.create(..., mounts={"/data": (dataset, "ro")})`.
- `restore` and `fork` attach clones, never the original.
- `workspace=None` means no volume at all (slightly faster, nothing persists).

### Durable snapshots and moving workspaces between hosts

On the Firecracker backend a whole sandbox, memory included, can be stored
too: `snap = sb.snapshot(durable=True)`, then `Sandbox.restore(snap.id)` on
any daemon that shares the store and runs the same CPU model and
Firecracker release resumes it with its processes running
([decision 0018](decisions/0018-durable-memory-snapshots.md)).

With a blob store configured (`vmlayerd --blob-store s3://bucket/prefix`, or
`az://`, `gs://`, `file://`), a snapshot can also be kept in object storage:

```python
ckpt = repo.snapshot(durable=True)    # uploads only blocks the store lacks
ckpt.delete(local_only=True)          # free local disk, keep it in the store

# On any host whose daemon uses the same store:
other = vmlayer.Client("unix:///run/vmlayer/vmlayerd.sock")
[s.id for s in other.list_volume_snapshots(remote=True)]
copy = Volume.from_snapshot(ckpt.id, client=other)   # downloads, then mounts

client.collect_garbage(dry_run=True)  # chunks no snapshot uses any more
```

A durable snapshot is stored as 4 MiB content-addressed chunks, so the next
snapshot of the same volume sends only the blocks that changed, and
identical blocks are stored once across all volumes. A Firecracker volume
can only be restored as a Firecracker volume and a Docker volume as a
Docker volume. A Firecracker volume restored from the store is usable at
once: blocks are fetched when the VM first reads them and in the background
([decision 0016](decisions/0016-lazy-volume-restore.md); needs
`modprobe nbd`, otherwise it is downloaded first). Docker volumes are
downloaded before they are attached. Design and limits are in
[decision 0011](decisions/0011-durable-snapshots-in-object-storage.md).

On Linux, a daemon that does not run as root can snapshot a Docker volume
only if it can read every file in it. A container running as root that
creates files readable only by root makes the snapshot fail with a
permission error; run the daemon as root, or the container as a user.

On macOS with Docker Desktop or colima, the daemon's data directory must be
under a path shared with the engine (the default `~/.vmlayer` is). The daemon
checks this at startup and marks the backend unavailable with a clear reason
if not.

## Errors

All exceptions derive from `vmlayer.VmlayerError`:

| Exception | When |
|---|---|
| `DaemonUnavailable` | no daemon at the endpoint |
| `SandboxNotFound`, `VolumeNotFound`, `SnapshotNotFound` | bad or stale id |
| `VolumeBusy` | single-writer rule |
| `BackendUnsupported` | the backend lacks the feature (`.backend`, `.feature`) |
| `ExecTimeout` | command exceeded its timeout (`.partial` has its output) |
| `TimeoutError` | `wait_for_port` and other daemon-side waits |
| `SnapshotIncompatible` | snapshot cannot run on this host |
| `InvalidArgument` | the daemon rejected the request |

## Concurrency

The SDK is thread-safe and the daemon serves many concurrent calls. Each
`Process` and `Pty` holds its own stream. An `asyncio` client is planned;
until then `asyncio.to_thread(sb.exec, ...)` works.

If the same process also starts subprocesses and captures their stderr (an
agent framework that shells out to `docker` or `git`, for example), gRPC's
fork handlers may write a line such as `FD from fork parent still in poll
list` into each child's stderr. It is harmless. Set
`GRPC_ENABLE_FORK_SUPPORT=0` (or `GRPC_VERBOSITY=ERROR`) in the environment
before importing `vmlayer` to turn it off; a subprocess that execs right
after the fork does not need gRPC's fork support.
