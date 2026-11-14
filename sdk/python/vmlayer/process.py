"""Handles for things running inside a sandbox: ``Process`` and ``Pty``."""

from __future__ import annotations

from collections.abc import Iterator
from datetime import timedelta
from typing import IO

from .types import ExecResult, OutputChunk, PtySize


class Process:
    """A command started with ``Sandbox.spawn``.

    Iterate the object to stream output chunks as they arrive. Call ``wait``
    to block until exit. Use ``stdin`` to feed input.
    """

    pid: int
    stdin: IO[bytes]

    def __iter__(self) -> Iterator[OutputChunk]:
        """Yield stdout/stderr chunks until the process exits."""
        raise NotImplementedError

    def wait(self, timeout: timedelta | None = None) -> ExecResult:
        """Block until exit. Buffers any output not already consumed via iteration."""
        raise NotImplementedError

    def kill(self, signal: int = 9) -> None:
        raise NotImplementedError


class Pty:
    """An interactive terminal session inside the sandbox.

    Unlike ``Process``, output is a single byte stream with terminal escapes
    intact, suitable for driving a TUI or attaching to xterm.js.
    """

    def __iter__(self) -> Iterator[bytes]:
        raise NotImplementedError

    def write(self, data: bytes) -> None:
        raise NotImplementedError

    def resize(self, size: PtySize) -> None:
        raise NotImplementedError

    def close(self) -> None:
        raise NotImplementedError
