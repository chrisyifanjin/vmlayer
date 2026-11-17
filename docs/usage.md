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
