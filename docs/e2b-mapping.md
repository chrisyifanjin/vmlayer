# E2B API mapping

## Run unmodified E2B code

`vmlayer-e2b-server` speaks E2B's wire protocols, so existing E2B programs
run against vmlayer by changing addresses only
([decision 0017](decisions/0017-e2b-wire-compatibility.md)):

```bash
pip install "vmlayer[e2b-server]"
vmlayer-e2b-server --listen 0.0.0.0:3000 --api-key "$KEY"
```

```bash
export E2B_API_URL=http://gateway:3000 E2B_SANDBOX_URL=http://gateway:3000 E2B_API_KEY="$KEY"
python my_e2b_program.py        # from e2b import Sandbox, unchanged
```

Tested with the E2B Python SDK, sync and async. Not supported, and answered
as unimplemented: PTY sessions, filesystem watchers, pause and resume,
E2B snapshots, metrics and logs, and E2B's template builds. Templates are
OCI images or labelled vmlayer snapshots.

## The Python facade

E2B's SDK shape is becoming the de facto interface for agent sandboxes: at
least two other self-hosted runtimes already ship E2B-compatible facades.
vmlayer keeps its own API (snapshot/fork and capabilities need a different
shape) and ships an E2B-shaped Python facade, `vmlayer.e2b`, so code written
for the E2B SDK runs on vmlayer after changing one import:

```python
# from e2b import Sandbox
from vmlayer.e2b import Sandbox, create_template

create_template("py-deps", "python:3.12-slim", setup="pip install requests")
with Sandbox.create("py-deps", timeout=300, metadata={"user": "u1"}) as sbx:
    print(sbx.commands.run("python3 -c 'import requests; print(requests.__version__)'").stdout)
```

It is source compatibility, not wire compatibility: the facade talks to a
vmlayer daemon, and an unmodified E2B SDK pointed at a vmlayer address does
not work. Templates are vmlayer snapshots labelled `e2b.template=<name>`,
made with `create_template` and restored on `Sandbox.create(name)`; any
other template string is used as an OCI image reference. `set_timeout`,
`pause` and `get_metrics` raise `NotImplementedError`.

| E2B (Python SDK) | vmlayer | Notes |
|---|---|---|
| `Sandbox.create(template, timeout=...)` | `Sandbox.create(image, timeout=...)` | vmlayer takes OCI refs directly; E2B templates ≈ vmlayer snapshots used as templates. |
| `Sandbox.connect(id)` | `Sandbox.connect(id)` | Same. |
| `sbx.commands.run(cmd)` | `sb.exec(cmd)` | Both return exit code + stdout + stderr. |
| `sbx.commands.run(cmd, background=True)` | `sb.spawn(cmd)` | vmlayer returns an iterable `Process`. |
| `handle.kill()` | `proc.kill()` | |
| `sbx.pty.create(...)` | `sb.pty(...)` | |
| `sbx.files.read / write / list / remove / exists` | `sb.files.read / write / list / remove / exists` | Same names on purpose. |
| `sbx.files.make_dir` | `sb.files.mkdir` | |
| `sbx.pause()` | `sb.snapshot(stay_paused=True)` or `sb.pause()` | E2B's pause persists to disk; vmlayer separates in-memory `pause` from persisted `snapshot`. |
| `Sandbox.connect(paused_id)` | `Sandbox.restore(snapshot)` | |
| — | `sb.fork(n)` | No E2B equivalent. |
| — | `sb.capabilities` | No E2B equivalent; E2B has one backend. |
| `sbx.get_host(port)` | `sb.expose(port).host` | |
| `sbx.set_timeout(...)` | not yet | Candidate for the API. |
| `sbx.kill()` | `sb.kill()` | |
| `Sandbox.list()` | `Client.list_sandboxes()` | |
| `sbx.get_metrics()` | not yet | Out of scope for step 0. |

Open question for the facade: E2B's `template` is a prebuilt image id managed
by their build service. The cheapest mapping is "a vmlayer snapshot with a
`template=` label", which gives E2B-style instant starts for free.
