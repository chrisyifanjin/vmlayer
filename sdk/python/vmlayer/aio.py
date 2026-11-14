"""``asyncio`` twin of the blocking API. Same shapes, each call awaitable."""

from __future__ import annotations

from collections.abc import AsyncIterator, Sequence
from datetime import timedelta

from .types import Backend, Capabilities, ExecResult, FileInfo, OutputChunk
from .volume import Snapshot


class AsyncSandbox:
    """``asyncio`` twin of ``Sandbox``. Same methods, each awaitable; streaming
    handles are async iterators. Only signatures that differ are listed; the
    rest mirror ``Sandbox`` one to one.
    """

    id: str
    backend: Backend
    capabilities: Capabilities
    files: AsyncFiles

    @classmethod
    async def create(cls, image: str, **kwargs: object) -> AsyncSandbox:
        raise NotImplementedError

    @classmethod
    async def restore(cls, snapshot: Snapshot | str, **kwargs: object) -> AsyncSandbox:
        raise NotImplementedError

    async def exec(self, cmd: str | Sequence[str], **kwargs: object) -> ExecResult:
        raise NotImplementedError

    async def spawn(self, cmd: str | Sequence[str], **kwargs: object) -> AsyncProcess:
        raise NotImplementedError

    async def snapshot(self, **kwargs: object) -> Snapshot:
        raise NotImplementedError

    async def fork(self, count: int = 1) -> list[AsyncSandbox]:
        raise NotImplementedError

    async def kill(self) -> None:
        raise NotImplementedError

    async def __aenter__(self) -> AsyncSandbox:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.kill()


class AsyncProcess:
    pid: int

    def __aiter__(self) -> AsyncIterator[OutputChunk]:
        raise NotImplementedError

    async def wait(self, timeout: timedelta | None = None) -> ExecResult:
        raise NotImplementedError

    async def kill(self, signal: int = 9) -> None:
        raise NotImplementedError


class AsyncFiles:
    async def read(self, path: str) -> bytes:
        raise NotImplementedError

    async def write(self, path: str, data: bytes | str, *, mode: int = 0o644) -> None:
        raise NotImplementedError

    async def list(self, path: str = "/") -> list[FileInfo]:
        raise NotImplementedError
