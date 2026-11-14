"""The ``sandbox.files`` namespace: filesystem operations through the guest agent."""

from __future__ import annotations

from pathlib import Path

from .types import FileInfo


class Files:
    """Filesystem operations inside a sandbox, reached as ``sandbox.files``.

    Paths are absolute paths inside the guest. Operations go through the guest
    agent, so they work while the sandbox is running or paused, and never
    touch the host filesystem directly.
    """

    def read(self, path: str) -> bytes:
        raise NotImplementedError

    def read_text(self, path: str, encoding: str = "utf-8") -> str:
        raise NotImplementedError

    def write(self, path: str, data: bytes | str, *, mode: int = 0o644) -> None:
        """Create or overwrite a file. Parent directories are created."""
        raise NotImplementedError

    def list(self, path: str = "/") -> list[FileInfo]:
        raise NotImplementedError

    def exists(self, path: str) -> bool:
        raise NotImplementedError

    def mkdir(self, path: str, *, parents: bool = True) -> None:
        raise NotImplementedError

    def remove(self, path: str, *, recursive: bool = False) -> None:
        raise NotImplementedError

    def upload(self, local: Path | str, remote: str) -> None:
        """Copy a local file or directory tree into the sandbox."""
        raise NotImplementedError

    def download(self, remote: str, local: Path | str) -> None:
        """Copy a file or directory tree out of the sandbox."""
        raise NotImplementedError
