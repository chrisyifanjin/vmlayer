"""The ``sandbox.files`` namespace: filesystem operations through the guest agent."""

from __future__ import annotations

from collections.abc import Iterator
from pathlib import Path
from typing import TYPE_CHECKING

from .errors import NotFound
from .types import FileInfo
from .v1 import common_pb2, files_pb2

if TYPE_CHECKING:
    from .client import Client

CHUNK = 64 * 1024


def _info(f: common_pb2.FileInfo) -> FileInfo:
    from . import _rpc

    return FileInfo(
        path=f.path,
        size=f.size,
        is_dir=f.is_dir,
        mode=f.mode,
        modified_at=_rpc.from_timestamp(f.modified_at),
    )


class Files:
    """Filesystem operations inside a sandbox, reached as ``sandbox.files``.

    Paths are absolute paths inside the guest. Operations go through the guest
    agent, so they work while the sandbox is running or paused, and never
    touch the host filesystem directly.
    """

    def __init__(self, client: Client, sandbox_id: str) -> None:
        self._client = client
        self._id = sandbox_id

    def read(self, path: str) -> bytes:
        c = self._client._conn
        parts = [
            bytes(ch.data)
            for ch in c.stream(
                c.files.Read, files_pb2.ReadFileRequest(sandbox_id=self._id, path=path)
            )
        ]
        return b"".join(parts)

    def read_text(self, path: str, encoding: str = "utf-8") -> str:
        return self.read(path).decode(encoding)

    def write(self, path: str, data: bytes | str, *, mode: int = 0o644) -> None:
        """Create or overwrite a file. Parent directories are created."""
        payload = data.encode("utf-8") if isinstance(data, str) else bytes(data)

        def gen() -> Iterator[files_pb2.WriteFileRequest]:
            yield files_pb2.WriteFileRequest(
                header=files_pb2.WriteFileRequest.Header(sandbox_id=self._id, path=path, mode=mode)
            )
            for i in range(0, len(payload), CHUNK):
                yield files_pb2.WriteFileRequest(data=payload[i : i + CHUNK])
            if not payload:
                yield files_pb2.WriteFileRequest(data=b"")

        c = self._client._conn
        try:
            c.files.Write(gen(), timeout=c.timeout)
        except Exception as e:
            from . import _rpc

            raise _rpc.translate(e) from None

    def list(self, path: str = "/") -> list[FileInfo]:
        c = self._client._conn
        r = c.call(c.files.List, files_pb2.ListFilesRequest(sandbox_id=self._id, path=path))
        return [_info(e) for e in r.entries]

    def stat(self, path: str) -> FileInfo:
        c = self._client._conn
        return _info(c.call(c.files.Stat, files_pb2.StatRequest(sandbox_id=self._id, path=path)))

    def exists(self, path: str) -> bool:
        try:
            self.stat(path)
            return True
        except NotFound:
            return False

    def mkdir(self, path: str, *, parents: bool = True) -> None:
        del parents  # the guest always creates parents
        c = self._client._conn
        c.call(c.files.Mkdir, files_pb2.MkdirRequest(sandbox_id=self._id, path=path))

    def remove(self, path: str, *, recursive: bool = False) -> None:
        c = self._client._conn
        c.call(
            c.files.Remove,
            files_pb2.RemoveRequest(sandbox_id=self._id, path=path, recursive=recursive),
        )

    def upload(self, local: Path | str, remote: str) -> None:
        """Copy a local file or directory tree into the sandbox."""
        src = Path(local)
        if src.is_dir():
            self.mkdir(remote)
            for child in sorted(src.rglob("*")):
                rel = child.relative_to(src).as_posix()
                target = f"{remote.rstrip('/')}/{rel}"
                if child.is_dir():
                    self.mkdir(target)
                elif child.is_file():
                    self.write(target, child.read_bytes(), mode=child.stat().st_mode & 0o7777)
        else:
            self.write(remote, src.read_bytes(), mode=src.stat().st_mode & 0o7777)

    def download(self, remote: str, local: Path | str) -> None:
        """Copy a file or directory tree out of the sandbox."""
        dst = Path(local)
        info = self.stat(remote)
        if info.is_dir:
            dst.mkdir(parents=True, exist_ok=True)
            for entry in self.list(remote):
                self.download(entry.path, dst / entry.name)
        else:
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.write_bytes(self.read(remote))
