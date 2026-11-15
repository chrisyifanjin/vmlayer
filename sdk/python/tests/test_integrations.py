"""The adapters, driven by the real frameworks against a live daemon.

Skipped when a framework is not installed (`uv sync --group integrations`).
"""

from __future__ import annotations

import asyncio

import pytest

import vmlayer

PY = "python:3.12-alpine"
SUBMIT = "echo COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT && python3 answer.py"


def test_minisweagent_real_agent_loop(daemon: vmlayer.Client) -> None:
    pytest.importorskip("minisweagent")
    from minisweagent.agents.default import DefaultAgent
    from minisweagent.models.test_models import DeterministicModel, make_output

    from vmlayer.integrations.minisweagent import VmlayerEnvironment

    env = VmlayerEnvironment(image=PY, env={"GREETING": "hi"}, timeout=20)
    try:
        # What the "model" does: write a file, run it, then submit its output.
        model = DeterministicModel(
            outputs=[
                make_output(
                    "write", [{"command": "echo 'print(6 * 7)' > answer.py && echo $GREETING"}]
                ),
                make_output("run", [{"command": "python3 answer.py"}]),
                make_output(
                    "submit",
                    [{"command": SUBMIT}],
                ),
            ]
        )
        agent = DefaultAgent(
            model,
            env,
            system_template="You are a test agent on {{system}}.",
            instance_template="Task: {{task}} (cwd={{cwd}}, image={{image}})",
            step_limit=5,
            cost_limit=10.0,
        )
        result = agent.run("compute the answer")
        assert result["exit_status"] == "Submitted"
        assert result["submission"].strip() == "42"
        observations = [m["content"] for m in agent.messages if m.get("role") == "user"]
        assert any("<returncode>0</returncode>" in o and "hi" in o for o in observations)
        assert any("42" in o for o in observations)
        # The template variables came from the environment's config.
        assert "image=python:3.12-alpine" in agent.messages[1]["content"]
        assert env.serialize()["info"]["config"]["environment_type"].endswith("VmlayerEnvironment")
    finally:
        env.cleanup()
    assert env.sandbox is None
    env.cleanup()  # idempotent


def test_minisweagent_timeout_and_errors(daemon: vmlayer.Client) -> None:
    pytest.importorskip("minisweagent")
    from vmlayer.integrations.minisweagent import VmlayerEnvironment

    env = VmlayerEnvironment(image=PY, timeout=2)
    try:
        ok = env.execute({"command": "echo out; echo err >&2; exit 3"})
        assert ok["returncode"] == 3 and "out" in ok["output"] and "err" in ok["output"]
        assert ok["exception_info"] == ""
        slow = env.execute({"command": "echo started; sleep 30"})
        assert slow["returncode"] == -1 and "started" in slow["output"]
        assert "timed out" in slow["exception_info"]
        assert env.execute({"command": "pwd"}, cwd="/tmp")["output"].strip() == "/tmp"
        # The underlying sandbox is there for snapshots.
        assert env.sandbox is not None and env.sandbox.capabilities.snapshot
    finally:
        env.cleanup()


def test_autogen_code_executor(daemon: vmlayer.Client) -> None:
    pytest.importorskip("autogen_core")
    from autogen_core import CancellationToken
    from autogen_core.code_executor import CodeBlock

    from vmlayer.integrations.autogen import VmlayerCodeExecutor

    async def scenario() -> None:
        async with VmlayerCodeExecutor(image=PY, timeout=5) as ex:
            token = CancellationToken()
            r = await ex.execute_code_blocks(
                [
                    CodeBlock(
                        language="python", code="open('state.txt', 'w').write('kept'); print(6 * 7)"
                    ),
                    CodeBlock(language="sh", code="cat state.txt; echo; echo from-shell"),
                ],
                token,
            )
            assert r.exit_code == 0
            assert "42" in r.output and "kept" in r.output and "from-shell" in r.output

            # State persists across calls in the same sandbox.
            r = await ex.execute_code_blocks(
                [CodeBlock(language="sh", code="cat state.txt")], token
            )
            assert r.output == "kept"

            # First failure stops the sequence.
            r = await ex.execute_code_blocks(
                [
                    CodeBlock(language="python", code="import sys; print('boom'); sys.exit(7)"),
                    CodeBlock(language="python", code="print('never')"),
                ],
                token,
            )
            assert r.exit_code == 7 and "boom" in r.output and "never" not in r.output

            # Timeout keeps partial output.
            r = await ex.execute_code_blocks(
                [CodeBlock(language="sh", code="echo started; sleep 30")], token
            )
            assert r.exit_code == 124 and "started" in r.output and "Timeout" in r.output

            with pytest.raises(ValueError):
                await ex.execute_code_blocks([CodeBlock(language="cobol", code="")], token)

            # Cancellation kills the running block.
            cancel = CancellationToken()
            task = asyncio.ensure_future(
                ex.execute_code_blocks([CodeBlock(language="sh", code="sleep 30")], cancel)
            )
            await asyncio.sleep(1.0)
            cancel.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert ex.sandbox is not None
            await asyncio.sleep(0.5)
            assert not any("sleep" in " ".join(p.argv) for p in ex.sandbox.processes())

            # restart() gives a clean sandbox.
            old = ex.sandbox.id
            await ex.restart()
            assert ex.sandbox is not None and ex.sandbox.id != old
            r = await ex.execute_code_blocks([CodeBlock(language="sh", code="ls state.txt")], token)
            assert r.exit_code != 0
        assert ex.sandbox is None

    asyncio.run(scenario())
