# E2B API mapping

E2B's SDK shape is becoming the de facto interface for agent sandboxes: at
least two other self-hosted runtimes already ship E2B-compatible facades.
vmlayer keeps its own API (snapshot/fork and capabilities need a different
shape) but tracks the mapping so a compatibility layer can be added later
without redesign.

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
