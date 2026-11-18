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

## Adapters

Adapters live in the Python SDK under `vmlayer.integrations` and import
their framework lazily; install with the matching extra.

| Adapter | Implements | Status |
|---|---|---|
| `vmlayer.integrations.minisweagent.VmlayerEnvironment` | mini-swe-agent `Environment` | **working**; tested by running a real `DefaultAgent` loop to submission (`pip install "vmlayer[minisweagent]"`) |
| `vmlayer.integrations.autogen.VmlayerCodeExecutor` | AutoGen `CodeExecutor` | **working**; tested with real `CodeBlock`s incl. timeout, cancellation, restart (`pip install "vmlayer[autogen]"`) |
| `vmlayer.integrations.deepagents.VmlayerSandbox` | Deep Agents `BaseSandbox` | **working**; host-side agent loop, Firecracker tool calls and file transfer (`pip install "vmlayer[deepagents]"`) |
| `vmlayer.integrations.openhands.VmlayerWorkspace` | OpenHands `RemoteWorkspace` | **experimental**; real agent-server image and command API ran in Firecracker (`pip install "vmlayer[openhands]"`, Python 3.12+) |
| `vmlayer.integrations.harbor.VmlayerEnvironment` | Harbor `BaseEnvironment` (Terminal-Bench and the other Harbor datasets, with any Harbor agent) | **working**; Harbor's own CLI runs trials with its oracle and no-op agents on Docker in CI and on Firecracker (nested KVM, a whole trial in 2 s); `no-network` and `allowlist` policies map to the sandbox's network (`pip install "vmlayer[harbor]"`, Python 3.12+) |
| Google ADK `BaseCodeExecutor` | write + exec | planned |
| SWE-ReX `AbstractDeployment` | boot swerex-server image, expose | planned |
| smolagents `RemotePythonExecutor` | boot kernel-gateway image, expose, Jupyter websocket | planned |

```python
from minisweagent.agents.default import DefaultAgent
from vmlayer.integrations.minisweagent import VmlayerEnvironment

env = VmlayerEnvironment(image="python:3.12", workspace="repo-42")   # volume survives the run
agent = DefaultAgent(model, env, **agent_config)
agent.run("Fix the failing test")
env.sandbox.snapshot()          # the vmlayer sandbox is right there
```

```python
from vmlayer.integrations.autogen import VmlayerCodeExecutor

async with VmlayerCodeExecutor(image="python:3.12") as executor:
    result = await executor.execute_code_blocks([CodeBlock(language="python", code="print(6*7)")], token)
```

For the host-side sandbox-as-tool pattern, Deep Agents keeps its loop and
model credentials outside the VM:

```python
from deepagents import create_deep_agent
from vmlayer.integrations.deepagents import VmlayerSandbox

with VmlayerSandbox(backend="firecracker") as backend:
    agent = create_deep_agent(model=my_model, backend=backend)
    result = agent.invoke({"messages": [{"role": "user", "content": "Inspect /work"}]})
```

The OpenHands adapter is a different pattern: it boots the framework's
agent-server image inside a VM. On the local nested-KVM host this took about
95 seconds to become healthy. It is a compatibility proof, not a fast-create
result; a warm snapshot of that 2 GiB VM did not fit on the 20 GiB test disk.

### Harbor

[Harbor](https://github.com/harbor-framework/harbor) runs any of its agents
against its datasets inside an environment. vmlayer is one, loaded by
import path:

```bash
pip install "vmlayer[harbor]"
harbor run --dataset terminal-bench@2.0 --agent claude-code \
  --env vmlayer.integrations.harbor:VmlayerEnvironment \
  --ek endpoint=tls://10.0.0.5:7443 --ek backend=firecracker
```

The adapter maps each piece of a Harbor task onto the sandbox:

- **Image.** The task's `docker_image` becomes the sandbox image, with no
  workspace volume, so the image's own working directory stays intact.
  Tasks that ship only a Dockerfile are built with the local
  `docker build`. The Docker backend can use such an image directly; for
  Firecracker, publish it and set `docker_image`.
- **Shape.** `cpus`, `memory_mb` and `storage_mb` become the sandbox
  shape.
- **Network.** `no-network` removes the network device. `allowlist`
  becomes the egress allowlist (Firecracker), and the adapter tells
  Harbor which kinds of entries it enforces.
- **Users and timeouts.** `exec(user=...)` runs as that user. A timeout
  raises the same error as Harbor's Docker environment.

Daemon endpoint, token and TLS come from `--ek endpoint=...` and the usual
`VMLAYER_*` variables.

### What plugging in found

Writing the first two adapters against the real packages changed the SDK
twice more:

- **Protobuf pinning.** `autogen-core` pins `protobuf~=5.29`; the SDK's
  checked-in stubs demanded 7.x and `import vmlayer` failed beside AutoGen.
  The stubs are now generated with an old pinned toolchain and the version
  pin is stripped ([decision 0010](decisions/0010-python-stub-compatibility.md));
  one wheel imports under protobuf 5, 6 and 7.
- **Cancellation needs `spawn`, not `exec`.** AutoGen cancels a running
  block through a token. A blocking `exec` cannot be interrupted from
  another task, so the adapter spawns, waits in a thread, and kills the
  process on cancel. `Process.kill` on a handle whose `wait` is in flight
  had to work, and does.
- **Both frameworks want stdout and stderr merged** in arrival order;
  `ExecResult.output` (added after the first survey) is what both adapters
  return.

## What this changes about priorities

- `expose` and `entrypoint` are on the critical path, not nice-to-haves.
  (Both shipped; agents that run whole inside the sandbox now also get the
  authenticated HTTPS gateway, decision 0021.)
- A snapshot of a sandbox with a warmed kernel gateway or agent-server is the
  single most valuable artifact for every one of these runtimes. Step 2
  (snapshot/restore) is the right thing to prove early.
- Nothing above needs `fork`. Fork is for the runtimes that do not exist yet
  (search over agent trajectories, RL rollouts); it stays first-class but is
  not what the first adapters will test.

## Agent Sandbox controller compatibility

There are two distinct integration targets:

| Target | Feasible path | Request path |
|---|---|---|
| Use the upstream controller and SDK unchanged | Choose a VM-backed `RuntimeClass` such as Kata in `Sandbox.spec.podTemplate.spec.runtimeClassName` | `SandboxClaim` → `Sandbox` → Pod → Kata VM |
| Use vmlayer's direct VM lifecycle while retaining a Kubernetes-facing Sandbox API | Write a separate reconciler or API adapter that maps Sandbox identity, readiness, termination, and commands to `vmlayerd` | API request → vmlayer host daemon → Firecracker VM |

The upstream core `SandboxReconciler` owns a Pod for every Sandbox; its create
path calls `reconcilePod`. Changing only `runtimeClassName` changes the Pod's
isolation engine, not the per-sandbox Kubernetes request path. A vmlayer
containerd runtime shim could also preserve the upstream controller, but it
would still create a Pod per sandbox and would require an OCI runtime shim.

The direct-VM option is **not implemented**. It cannot transparently promise
all upstream semantics: `podTemplate`, Pod IPs, Services, `kubectl exec`, PVCs,
and warm-pool claim adoption are Pod-oriented. A compatibility adapter should
declare which fields and operations it supports and reject the others. The
first useful subset would be create, Ready, exec/files through a gateway,
delete, and a stable sandbox ID. It would need explicit conflict avoidance
with the upstream controller if both watched the same CRD.

Upstream references: [core reconciler](https://github.com/kubernetes-sigs/agent-sandbox/blob/main/controllers/sandbox_controller.go),
[Kata example](https://github.com/kubernetes-sigs/agent-sandbox/blob/main/examples/kata-aks-sandbox/README.md),
[Python SDK](https://github.com/kubernetes-sigs/agent-sandbox/blob/main/clients/python/agentic-sandbox-client/README.md).

Sources read: OpenHands `software-agent-sdk` `workspace/base.py`,
`workspace/models.py`, `workspace/remote/base.py`; SWE-ReX
`runtime/abstract.py`; mini-swe-agent `__init__.py`; smolagents
`remote_executors.py`; autogen-core `code_executor/_base.py`; adk-python
`code_executors/base_code_executor.py`.
