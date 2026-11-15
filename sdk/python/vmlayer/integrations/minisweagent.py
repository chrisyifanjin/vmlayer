"""mini-swe-agent environment backed by a vmlayer sandbox.

Drop-in for ``minisweagent.environments.docker.DockerEnvironment``::

    from minisweagent.agents.default import DefaultAgent
    from vmlayer.integrations.minisweagent import VmlayerEnvironment

    env = VmlayerEnvironment(image="python:3.12")
    agent = DefaultAgent(model, env, **agent_config)
    agent.run("Fix the failing test in /work/repo")

The sandbox is created on construction and killed by ``cleanup()`` (also
called when the object is garbage collected). ``env.sandbox`` is the
underlying :class:`vmlayer.Sandbox`, so snapshots and volumes are available
to the caller: ``env.sandbox.snapshot()`` before a risky step, a named
``workspace`` to keep the repository between runs.
"""

from __future__ import annotations

import os
import platform
from datetime import timedelta
from typing import Any

from minisweagent.exceptions import Submitted
from minisweagent.utils.serialize import recursive_merge
from pydantic import BaseModel

from ..client import Client
from ..errors import ExecTimeout, VmlayerError
from ..sandbox import Sandbox
from ..types import AUTO, Backend

SUBMIT_MARKER = "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT"


class VmlayerEnvironmentConfig(BaseModel):
    image: str
    cwd: str = ""
    """Working directory for commands. Empty means the sandbox's workdir."""
    env: dict[str, str] = {}
    """Environment variables set for every command."""
    forward_env: list[str] = []
    """Host environment variables forwarded to every command, if set."""
    timeout: int = 30
    """Per-command timeout in seconds."""
    interpreter: list[str] = ["/bin/sh", "-c"]
    """How a command string is run. Use ``["bash", "-lc"]`` for images with bash."""
    cpus: int | None = None
    memory_mb: int | None = None
    backend: str = "auto"
    """``auto``, ``docker`` or ``firecracker``."""
    workspace: str | None = None
    """Name of a vmlayer volume to mount at the workdir; kept after cleanup."""
    sandbox_timeout: int = 3600
    """Hard lifetime of the sandbox in seconds (0 disables)."""


class VmlayerEnvironment:
    """Executes mini-swe-agent actions inside a vmlayer sandbox."""

    def __init__(
        self,
        *,
        config_class: type = VmlayerEnvironmentConfig,
        client: Client | None = None,
        **kwargs: Any,
    ) -> None:
        self.config = config_class(**kwargs)
        self.sandbox: Sandbox | None = Sandbox.create(
            self.config.image,
            cpus=self.config.cpus,
            memory_mb=self.config.memory_mb,
            workspace=self.config.workspace if self.config.workspace else AUTO,
            entrypoint=None,
            timeout=timedelta(seconds=self.config.sandbox_timeout)
            if self.config.sandbox_timeout
            else None,
            backend=Backend(self.config.backend),
            labels={"created-by": "mini-swe-agent"},
            client=client,
        )

    def execute(
        self, action: dict[str, Any], cwd: str = "", *, timeout: int | None = None
    ) -> dict[str, Any]:
        """Run ``action["command"]`` and return mini-swe-agent's result dict."""
        assert self.sandbox is not None, "environment already cleaned up"
        command = action.get("command", "")
        env = dict(self.config.env)
        for key in self.config.forward_env:
            if (value := os.getenv(key)) is not None:
                env[key] = value
        seconds = timeout or self.config.timeout
        output: dict[str, Any]
        try:
            result = self.sandbox.exec(
                [*self.config.interpreter, command],
                cwd=cwd or self.config.cwd or None,
                env=env,
                timeout=timedelta(seconds=seconds),
            )
            output = {"output": result.output, "returncode": result.exit_code, "exception_info": ""}
        except ExecTimeout as e:
            output = {
                "output": e.partial.output,
                "returncode": -1,
                "exception_info": (
                    f"An error occurred while executing the command: timed out after {seconds}s"
                ),
                "extra": {"exception_type": "TimeoutExpired", "exception": str(e)},
            }
        except VmlayerError as e:
            output = {
                "output": "",
                "returncode": -1,
                "exception_info": f"An error occurred while executing the command: {e}",
                "extra": {"exception_type": type(e).__name__, "exception": str(e)},
            }
        self._check_finished(output)
        return output

    def _check_finished(self, output: dict[str, Any]) -> None:
        """Raise ``Submitted`` when the command printed the submit marker first."""
        lines = output.get("output", "").lstrip().splitlines(keepends=True)
        if lines and lines[0].strip() == SUBMIT_MARKER and output["returncode"] == 0:
            submission = "".join(lines[1:])
            raise Submitted(
                {
                    "role": "exit",
                    "content": submission,
                    "extra": {"exit_status": "Submitted", "submission": submission},
                }
            )

    def get_template_vars(self, **kwargs: Any) -> dict[str, Any]:
        merged: dict[str, Any] = recursive_merge(
            self.config.model_dump(), platform.uname()._asdict(), kwargs
        )
        return merged

    def serialize(self) -> dict[str, Any]:
        return {
            "info": {
                "config": {
                    "environment": self.config.model_dump(mode="json"),
                    "environment_type": f"{self.__class__.__module__}.{self.__class__.__name__}",
                }
            }
        }

    def cleanup(self) -> None:
        """Kill the sandbox. A named ``workspace`` volume is kept."""
        sandbox, self.sandbox = getattr(self, "sandbox", None), None
        if sandbox is not None:
            try:
                sandbox.kill()
            except VmlayerError:
                pass

    def __del__(self) -> None:
        self.cleanup()
