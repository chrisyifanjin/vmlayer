# Plugging vmlayer into existing agent runtimes

The test of "is this API useful" is whether real agent frameworks can sit on
it without contortions. This document reads the sandbox interface of six
open-source runtimes (Nov 2026), maps each onto vmlayer, and records the gaps
that changed the API.

## What the runtimes ask for

| Runtime | Sandbox interface | Shape |
|---|---|---|
| **OpenHands** (software-agent-sdk) | `BaseWorkspace`: `execute_command(cmd, cwd, timeout) -> CommandResult{exit_code, stdout, stderr, timeout_occurred}`, `file_upload(path\|bytes)`, `file_download`, `git_changes`, `git_diff`, `pause`, `resume`. `RemoteWorkspace` talks HTTP to an **agent-server running inside the sandbox**: `start_command -> id`, `get_command_output(id)`, `stop_command(id)`, `/health`. | sandbox is a server |
| **SWE-agent** (SWE-ReX) | `AbstractRuntime`: `is_alive`, `create_session` (named bash session, PS1-based boundaries), `run_in_session(BashAction{command, session, timeout, is_interactive_command, expect})`, `execute(Command{command, timeout, shell, env, cwd, merge_output_streams})`, `read_file`, `write_file`, `upload`, `close`. A `Deployment` boots a container running the **swerex server** and `RemoteRuntime` talks to it. | sandbox is a server, plus persistent shell sessions |
| **mini-swe-agent** | `Environment` protocol: `execute(action: dict, cwd) -> dict`. One method. | bare exec |
| **smolagents** | `RemotePythonExecutor.run_code_raise_errors(code) -> CodeOutput`. Every concrete executor (Docker, Modal, Blaxel) boots an image with **Jupyter Kernel Gateway**, exposes the port and speaks the Jupyter websocket protocol. E2B uses E2B's own `run_code`. No file transfer. | sandbox is a Jupyter server |
| **AutoGen** (autogen-core) | `CodeExecutor`: `execute_code_blocks([CodeBlock{code, language}], cancellation_token) -> CodeResult{exit_code, output}`, `start`, `stop`, `restart`. | write file, exec, cancel |
| **Google ADK** | `BaseCodeExecutor.execute_code(ctx, input) -> result`; `stateful: bool`, `timeout_seconds`, retry count, CSV input files. | exec, optionally stateful |

## The common core

Every one of them reduces to:

```
execute(command, cwd, timeout) -> (exit_code, stdout, stderr, timed_out)
read / write / upload / download files
kill
```

`Sandbox.exec`, `Sandbox.files` and `Sandbox.kill` cover this directly. Result
field names were chosen to match (`exit_code`, `stdout`, `stderr`).

## The pattern nobody's API makes easy: "the sandbox is a server"

Four of six (OpenHands, SWE-ReX, smolagents, and ADK's stateful mode via a
kernel) do not want to exec commands one at a time. They want to **boot an
image that runs a long-lived server, get a URL to it, and talk to that
server with their own protocol**. The sandbox provider's job is then just:
start it, expose a port, tell me when it is ready, keep it alive, kill it.

vmlayer's first draft had no way to run something at boot other than
`spawn` after `create`. That was the biggest gap found. Changes made:

1. `Sandbox.create(entrypoint=...)`: a command started by the guest agent as
   soon as the sandbox is up. Defaults to the OCI image's `ENTRYPOINT`/`CMD`,
   so a `jupyter/kernel-gateway` or `swerex-server` image just works.
2. `Sandbox.wait_for_port(port, timeout)`: block until something listens
   inside the guest. Combined with `expose(port)` this gives the
   "boot, expose, ready" sequence in three lines.
3. Readiness is observable: `state` moves to `RUNNING` only after the guest
   agent answers, so `exec` is never racing boot.

## Other gaps, and what was done

| Gap | Who needs it | Decision |
|---|---|---|
| Re-attach to a background process by id after the client reconnects | OpenHands (`get_command_output(id)`) | Added `Sandbox.processes()` and `Sandbox.process(pid)`. `spawn` handles are now re-obtainable. |
| Persistent shell session with state (cwd, env, shell vars) across commands, interactive programs like gdb | SWE-ReX sessions | Not a daemon feature. Build `vmlayer.shell.Session` in the SDK on top of `pty()` with sentinel-based boundaries, mirroring SWE-ReX's PS1 approach. Listed under adapters. |
| Interleaved stdout/stderr in arrival order | SWE-ReX `merge_output_streams`, all LLM observation formatting | Added `ExecResult.output`: both streams merged in arrival order. The daemon already has the order from the chunk stream. |
| Timed-out vs failed distinction in the result rather than an exception | OpenHands `timeout_occurred` | Kept the exception (`ExecTimeout`); it carries the partial output. Adapters translate. |
| Git operations | OpenHands `git_changes`, `git_diff` | Out of scope (ADR 0006 D1). Adapter runs `git` through `exec`. |
| Jupyter protocol, rich outputs (images) | smolagents, ADK | Out of core. Adapter boots a kernel-gateway image and speaks Jupyter over `expose(8888)`, exactly as the Docker/Modal executors do today. |
| Cancellation token | AutoGen | `Process.kill()`. |
| Output-based (idle) timeout instead of wall clock | SWE-ReX | Not added. Adapters implement idle timeouts on the `spawn` stream. |

## Planned adapters

Each is a small package under `integrations/`, with its own tests against a
live daemon. Estimated size is after the daemon exists; none can be written
before roadmap step 4.

| Adapter | Implements | Pattern | Size |
|---|---|---|---|
| `vmlayer-minisweagent` | `Environment` | exec | ~30 lines |
| `vmlayer-autogen` | `CodeExecutor` | write + exec + kill | ~80 lines |
| `vmlayer-adk` | `BaseCodeExecutor` | write + exec | ~80 lines |
| `vmlayer-swerex` | `AbstractDeployment` | boot swerex-server image, expose, point `RemoteRuntime` at it | ~120 lines |
| `vmlayer-openhands` | `BaseWorkspace` | boot agent-server image, expose, delegate to `RemoteWorkspace` | ~150 lines |
| `vmlayer-smolagents` | `RemotePythonExecutor` | boot kernel-gateway image, expose, Jupyter websocket | ~150 lines |

The order above is also the validation order: the exec-shaped ones first,
because they exercise the basic path, then the server-shaped ones, because
they exercise `entrypoint` + `expose` + `wait_for_port` + snapshots of a
warmed-up server (the thing that makes agent cold starts fast).

## What this changes about priorities

- `expose` and `entrypoint` are on the critical path, not nice-to-haves.
  Networking moves earlier in roadmap step 4.
- A snapshot of a sandbox with a warmed kernel gateway or agent-server is the
  single most valuable artifact for every one of these runtimes. Step 2
  (snapshot/restore) is the right thing to prove early.
- Nothing above needs `fork`. Fork is for the runtimes that do not exist yet
  (search over agent trajectories, RL rollouts); it stays first-class but is
  not what the first adapters will test.

Sources read: OpenHands `software-agent-sdk` `workspace/base.py`,
`workspace/models.py`, `workspace/remote/base.py`; SWE-ReX
`runtime/abstract.py`; mini-swe-agent `__init__.py`; smolagents
`remote_executors.py`; autogen-core `code_executor/_base.py`; adk-python
`code_executors/base_code_executor.py`.
