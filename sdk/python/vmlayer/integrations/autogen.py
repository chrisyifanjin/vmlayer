"""AutoGen code executor backed by a vmlayer sandbox.

Use anywhere AutoGen accepts a ``CodeExecutor``::

    from autogen_core import CancellationToken
    from autogen_core.code_executor import CodeBlock
    from vmlayer.integrations.autogen import VmlayerCodeExecutor

    async with VmlayerCodeExecutor(image="python:3.12") as executor:
        result = await executor.execute_code_blocks(
            [CodeBlock(language="python", code="print(6 * 7)")], CancellationToken()
        )
        print(result.exit_code, result.output)

State persists between calls (files, installed packages) because every
block runs in the same sandbox; ``restart()`` gives a fresh one.
``executor.sandbox`` exposes the underlying :class:`vmlayer.Sandbox`.
"""

from __future__ import annotations

import asyncio
import hashlib
from datetime import timedelta
from typing import Any

from autogen_core import CancellationToken
from autogen_core.code_executor import CodeBlock, CodeExecutor, CodeResult

from ..client import Client
from ..errors import ExecTimeout
from ..sandbox import Sandbox
from ..types import Backend

# language -> (file extension, argv prefix)
RUNNERS: dict[str, tuple[str, list[str]]] = {
    "python": ("py", ["python3"]),
    "py": ("py", ["python3"]),
    "bash": ("sh", ["bash"]),
    "sh": ("sh", ["sh"]),
    "shell": ("sh", ["sh"]),
    "javascript": ("js", ["node"]),
    "js": ("js", ["node"]),
}


class VmlayerCodeExecutor(CodeExecutor):  # type: ignore[misc]
    """Runs AutoGen code blocks in one long-lived vmlayer sandbox."""

    def __init__(
        self,
        image: str = "python:3.12-slim",
        *,
        timeout: int = 60,
        work_dir: str = "",
        cpus: int | None = None,
        memory_mb: int | None = None,
        backend: str = "auto",
        sandbox_timeout: int = 3600,
        client: Client | None = None,
    ) -> None:
        """``timeout`` is per block, in seconds. ``work_dir`` defaults to the
        sandbox's workdir. ``sandbox_timeout`` is the sandbox's hard lifetime."""
        self._image = image
        self._timeout = timeout
        self._work_dir = work_dir
        self._create_kwargs: dict[str, Any] = {
            "cpus": cpus,
            "memory_mb": memory_mb,
            "backend": Backend(backend),
            "timeout": timedelta(seconds=sandbox_timeout) if sandbox_timeout else None,
            "entrypoint": None,
            "labels": {"created-by": "autogen"},
            "client": client,
        }
        self.sandbox: Sandbox | None = None

    async def start(self) -> None:
        if self.sandbox is None:
            self.sandbox = await asyncio.to_thread(
                Sandbox.create, self._image, **self._create_kwargs
            )

    async def stop(self) -> None:
        sandbox, self.sandbox = self.sandbox, None
        if sandbox is not None:
            await asyncio.to_thread(sandbox.kill)

    async def restart(self) -> None:
        await self.stop()
        await self.start()

    async def execute_code_blocks(
        self, code_blocks: list[CodeBlock], cancellation_token: CancellationToken
    ) -> CodeResult:
        """Run blocks in order; stop at the first failure.

        Raises ``ValueError`` for an unsupported language. A block that
        exceeds the timeout yields exit code 124 with whatever it printed.
        Cancelling the token kills the running block.
        """
        if self.sandbox is None:
            await self.start()
        sandbox = self.sandbox
        assert sandbox is not None
        workdir = self._work_dir or sandbox.workdir
        outputs: list[str] = []
        exit_code = 0
        for block in code_blocks:
            lang = block.language.lower()
            if lang not in RUNNERS:
                raise ValueError(f"unsupported language: {block.language}")
            ext, runner = RUNNERS[lang]
            digest = hashlib.sha256(block.code.encode()).hexdigest()[:12]
            path = f"{workdir.rstrip('/')}/tmp_code_{digest}.{ext}"
            await asyncio.to_thread(sandbox.files.write, path, block.code)
            proc = await asyncio.to_thread(
                sandbox.spawn,
                [*runner, path],
                cwd=workdir,
                timeout=timedelta(seconds=self._timeout),
            )
            waiter = asyncio.ensure_future(asyncio.to_thread(proc.wait))
            cancellation_token.link_future(waiter)
            try:
                result = await waiter
            except asyncio.CancelledError:
                await asyncio.to_thread(proc.kill)
                raise
            except ExecTimeout as e:
                outputs.append(e.partial.output + f"\nTimeout: exceeded {self._timeout}s")
                exit_code = 124
                break
            outputs.append(result.output)
            exit_code = result.exit_code
            if exit_code != 0:
                break
        return CodeResult(exit_code=exit_code, output="".join(outputs))
