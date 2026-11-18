"""asyncio client: ``AsyncClient`` and ``AsyncSandbox`` on ``grpc.aio``.

The same API as the synchronous SDK, with ``await``::

    from vmlayer.aio import AsyncSandbox

    async with await AsyncSandbox.create("python:3.12") as sb:
        r = await sb.exec("python -c 'print(6*7)'")
        await sb.files.write("/work/main.py", "print('hi')")
        async for chunk in await sb.spawn("python /work/main.py"):
            print(chunk.text, end="")

Calls never block the event loop: every RPC is native ``grpc.aio``. Request
building and result parsing are shared with the synchronous classes, so
both behave the same. Interactive terminals (``pty``) and volume
management are sync-only for now; volumes can still be passed to
``AsyncSandbox.create`` by name or ID.
"""

from __future__ import annotations

import asyncio
import weakref
from collections.abc import AsyncIterator, Callable, Mapping, Sequence
from datetime import timedelta
from pathlib import Path
from typing import Any, Literal, TypeVar

import grpc
import grpc.aio

from . import _rpc
from .client import _system_info
from .errors import NotFound
from .files import CHUNK, _info
from .process import _chunk, _Collector
from .sandbox import (
    _create_request,
    _endpoint,
    _exec_request,
    _ExecFold,
    _Fields,
    _process_info,
)
from .types import (
    AUTO,
    Backend,
    Endpoint,
    ExecResult,
    FileInfo,
    IoLimits,
    OutputChunk,
    ProcessInfo,
    SandboxState,
    SystemInfo,
)
from .v1 import (
    common_pb2,
    files_pb2,
    files_pb2_grpc,
    processes_pb2,
    processes_pb2_grpc,
    sandboxes_pb2,
    sandboxes_pb2_grpc,
    system_pb2,
    system_pb2_grpc,
)
from .volume import Snapshot, Volume

__all__ = ["AsyncClient", "AsyncFiles", "AsyncProcess", "AsyncSandbox", "default_client"]

T = TypeVar("T")

_STATES = {
    common_pb2.SANDBOX_STATE_CREATING: SandboxState.CREATING,
    common_pb2.SANDBOX_STATE_RUNNING: SandboxState.RUNNING,
    common_pb2.SANDBOX_STATE_PAUSED: SandboxState.PAUSED,
    common_pb2.SANDBOX_STATE_STOPPED: SandboxState.STOPPED,
    common_pb2.SANDBOX_STATE_FAILED: SandboxState.FAILED,
}


class _Connection:
    """One ``grpc.aio`` channel and its stubs."""

    def __init__(
        self, endpoint: str | None, token: str | None, tls: _rpc.TLS | None, timeout: timedelta
    ) -> None:
        spec = _rpc.ChannelSpec(endpoint, token, tls)
        self.endpoint = spec.endpoint
        self.timeout = timeout.total_seconds()
        if spec.credentials is not None:
            self.channel = grpc.aio.secure_channel(
                spec.target, spec.credentials, options=spec.options
            )
        else:
            self.channel = grpc.aio.insecure_channel(spec.target, options=spec.options)
        self.sandboxes = sandboxes_pb2_grpc.SandboxesStub(self.channel)  # type: ignore[no-untyped-call]
        self.processes = processes_pb2_grpc.ProcessesStub(self.channel)  # type: ignore[no-untyped-call]
        self.files = files_pb2_grpc.FilesStub(self.channel)  # type: ignore[no-untyped-call]
        self.system = system_pb2_grpc.SystemStub(self.channel)  # type: ignore[no-untyped-call]

    async def call(
        self, method: Callable[..., Any], request: Any, *, timeout: float | None = None
    ) -> Any:
        try:
            return await method(request, timeout=self.timeout if timeout is None else timeout)
        except grpc.RpcError as e:
            raise _rpc.translate(e) from None

    async def stream(
        self, method: Callable[..., Any], request: Any, *, timeout: float | None = None
    ) -> AsyncIterator[Any]:
        try:
            async for item in method(request, timeout=timeout):
                yield item
        except grpc.RpcError as e:
            raise _rpc.translate(e) from None


class AsyncClient:
    """Connection to a ``vmlayerd`` daemon for asyncio code.

    Takes the same ``endpoint``, ``token`` and ``tls`` as ``vmlayer.Client``.
    Create it inside the event loop that uses it; ``grpc.aio`` channels
    belong to one loop.
    """

    def __init__(
        self,
        endpoint: str | None = None,
        *,
        token: str | None = None,
        tls: _rpc.TLS | None = None,
        timeout: timedelta = timedelta(seconds=30),
    ) -> None:
        self._conn = _Connection(endpoint, token, tls, timeout)

    @property
    def endpoint(self) -> str:
        return self._conn.endpoint

    async def info(self) -> SystemInfo:
        c = self._conn
        return _system_info(await c.call(c.system.Info, common_pb2.Empty()))

    async def health(self) -> bool:
        c = self._conn
        return bool((await c.call(c.system.Health, common_pb2.Empty())).ok)

    async def list_sandboxes(
        self, *, labels: Mapping[str, str] | None = None
    ) -> list[AsyncSandbox]:
        c = self._conn
        r = await c.call(
            c.sandboxes.List, sandboxes_pb2.ListSandboxesRequest(labels=_rpc.labels_arg(labels))
        )
        return [AsyncSandbox(self, pb) for pb in r.sandboxes]

    async def list_snapshots(
        self, *, labels: Mapping[str, str] | None = None, remote: bool = False
    ) -> list[Snapshot]:
        c = self._conn
        r = await c.call(
            c.system.ListSnapshots,
            system_pb2.ListSnapshotsRequest(labels=_rpc.labels_arg(labels), remote=remote),
            timeout=300,
        )
        return [_snapshot(s) for s in r.snapshots]

    async def delete_snapshot(self, snapshot: Snapshot | str, *, local_only: bool = False) -> None:
        c = self._conn
        sid = snapshot if isinstance(snapshot, str) else snapshot.id
        await c.call(
            c.system.DeleteSnapshot,
            system_pb2.DeleteSnapshotRequest(id=sid, local_only=local_only),
        )

    async def close(self) -> None:
        await self._conn.channel.close()

    async def __aenter__(self) -> AsyncClient:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()


_defaults: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, AsyncClient] = (
    weakref.WeakKeyDictionary()
)


def default_client() -> AsyncClient:
    """The client used when none is passed: one per event loop, configured
    from ``VMLAYER_ENDPOINT``, ``VMLAYER_TOKEN`` and ``VMLAYER_TLS_*``."""
    loop = asyncio.get_running_loop()
    client = _defaults.get(loop)
    if client is None:
        client = _defaults[loop] = AsyncClient()
    return client


def _snapshot(s: common_pb2.Snapshot) -> Snapshot:
    # Snapshot's own methods are synchronous; use AsyncClient.delete_snapshot.
    return Snapshot._from_pb(None, s)  # type: ignore[arg-type]


class AsyncSandbox(_Fields):
    """One isolated environment, for asyncio code. See ``vmlayer.Sandbox``
    for what each operation does; the semantics are identical.

    ``async with`` kills the sandbox on exit.
    """

    def __init__(self, client: AsyncClient, pb: common_pb2.Sandbox) -> None:
        self._client = client
        self._pb = pb
        self.files = AsyncFiles(client, pb.id)

    def __repr__(self) -> str:
        return f"AsyncSandbox(id={self.id!r}, image={self.image!r}, backend={self._pb.backend!r})"

    @property
    def state(self) -> SandboxState:
        """State as of the last ``refresh`` (or creation)."""
        return _STATES.get(self._pb.state, SandboxState.FAILED)

    @property
    def exposed(self) -> Mapping[int, Endpoint]:
        """Exposed guest ports as of the last ``refresh``."""
        return {int(p): Endpoint(e.host, e.port) for p, e in self._pb.exposed.items()}

    async def refresh(self) -> AsyncSandbox:
        c = self._client._conn
        self._pb = await c.call(c.sandboxes.Get, sandboxes_pb2.GetSandboxRequest(id=self.id))
        return self

    # -- construction -------------------------------------------------------

    @classmethod
    async def create(
        cls,
        image: str,
        *,
        cpus: int | None = None,
        memory_mb: int | None = None,
        disk_mb: int | None = None,
        env: Mapping[str, str] | None = None,
        workdir: str | None = None,
        workspace: Volume | str | type[AUTO] | None = AUTO,
        mounts: Mapping[str, Volume | tuple[Volume, Literal["ro", "rw"]]] | None = None,
        entrypoint: Sequence[str] | type[AUTO] | None = AUTO,
        timeout: timedelta | None = timedelta(minutes=15),
        backend: Backend = Backend.AUTO,
        network: bool = True,
        egress: Sequence[str] | None = None,
        limits: IoLimits | None = None,
        lease: str | None = None,
        reschedule: bool = False,
        labels: Mapping[str, str] | None = None,
        credentials: Sequence[str] = (),
        client: AsyncClient | None = None,
    ) -> AsyncSandbox:
        """Boot a new sandbox from an image (see ``vmlayer.Sandbox.create``)."""
        cl = client or default_client()
        req = _create_request(
            image,
            cpus=cpus,
            memory_mb=memory_mb,
            disk_mb=disk_mb,
            env=env,
            workdir=workdir,
            workspace=workspace,
            mounts=mounts,
            entrypoint=entrypoint,
            timeout=timeout,
            backend=backend,
            network=network,
            egress=egress,
            limits=limits,
            lease=lease,
            reschedule=reschedule,
            labels=labels,
            credentials=credentials,
        )
        return cls(cl, await cl._conn.call(cl._conn.sandboxes.Create, req, timeout=600))

    @classmethod
    async def restore(
        cls,
        snapshot: Snapshot | str,
        *,
        timeout: timedelta | None = timedelta(minutes=15),
        labels: Mapping[str, str] | None = None,
        client: AsyncClient | None = None,
    ) -> AsyncSandbox:
        """Start a new sandbox from a snapshot (see ``vmlayer.Sandbox.restore``)."""
        cl = client or default_client()
        sid = snapshot if isinstance(snapshot, str) else snapshot.id
        req = sandboxes_pb2.RestoreSandboxRequest(
            snapshot_id=sid, ttl=_rpc.to_duration(timeout), labels=_rpc.labels_arg(labels)
        )
        return cls(cl, await cl._conn.call(cl._conn.sandboxes.Restore, req, timeout=600))

    @classmethod
    async def connect(cls, sandbox_id: str, *, client: AsyncClient | None = None) -> AsyncSandbox:
        """Attach to a sandbox that already exists on the daemon."""
        cl = client or default_client()
        req = sandboxes_pb2.GetSandboxRequest(id=sandbox_id)
        return cls(cl, await cl._conn.call(cl._conn.sandboxes.Get, req))

    # -- commands -------------------------------------------------------------

    async def exec(
        self,
        cmd: str | Sequence[str],
        *,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        stdin: bytes | None = None,
        timeout: timedelta | None = timedelta(minutes=5),
        user: str | None = None,
    ) -> ExecResult:
        """Run a command to completion. A non-zero exit code is not an
        exception; ``ExecTimeout`` is raised when ``timeout`` elapses."""
        c = self._client._conn
        req = _exec_request(self.id, cmd, cwd=cwd, env=env, timeout=timeout, stdin=stdin, user=user)
        fold = _ExecFold()
        rpc_timeout = (timeout.total_seconds() + 30) if timeout else None
        async for ev in c.stream(c.processes.Exec, req, timeout=rpc_timeout):
            fold.feed(ev)
        return fold.result(timeout)

    async def spawn(
        self,
        cmd: str | Sequence[str],
        *,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        timeout: timedelta | None = None,
        user: str | None = None,
    ) -> AsyncProcess:
        """Start a command and return at once with a streaming handle."""
        c = self._client._conn
        req = _exec_request(self.id, cmd, cwd=cwd, env=env, timeout=timeout, detach=True, user=user)
        pid = 0
        async for ev in c.stream(c.processes.Exec, req, timeout=c.timeout):
            if ev.WhichOneof("event") == "started":
                pid = ev.started.pid
                break
        return AsyncProcess(self._client, self.id, pid)

    async def processes(self) -> list[ProcessInfo]:
        c = self._client._conn
        r = await c.call(c.processes.List, processes_pb2.ListProcessesRequest(sandbox_id=self.id))
        return [_process_info(p) for p in r.processes]

    def process(self, pid: int) -> AsyncProcess:
        """Re-attach to a running process by pid. ``process(1)`` is the entrypoint."""
        return AsyncProcess(self._client, self.id, pid)

    # -- state ----------------------------------------------------------------

    async def snapshot(
        self,
        *,
        labels: Mapping[str, str] | None = None,
        stay_paused: bool = False,
        durable: bool = False,
    ) -> Snapshot:
        c = self._client._conn
        req = sandboxes_pb2.SnapshotSandboxRequest(
            id=self.id, labels=_rpc.labels_arg(labels), stay_paused=stay_paused, durable=durable
        )
        return _snapshot(await c.call(c.sandboxes.TakeSnapshot, req, timeout=3600))

    async def fork(self, count: int = 1) -> list[AsyncSandbox]:
        c = self._client._conn
        req = sandboxes_pb2.ForkSandboxRequest(id=self.id, count=count)
        r = await c.call(c.sandboxes.Fork, req, timeout=600)
        return [AsyncSandbox(self._client, child) for child in r.children]

    async def pause(self) -> None:
        c = self._client._conn
        await c.call(c.sandboxes.Pause, sandboxes_pb2.PauseSandboxRequest(id=self.id))

    async def resume(self) -> None:
        c = self._client._conn
        await c.call(c.sandboxes.Resume, sandboxes_pb2.ResumeSandboxRequest(id=self.id))

    async def kill(self) -> None:
        """Destroy the sandbox and release everything. Idempotent."""
        c = self._client._conn
        await c.call(c.sandboxes.Kill, sandboxes_pb2.KillSandboxRequest(id=self.id), timeout=120)

    # -- network --------------------------------------------------------------

    async def expose(self, port: int, *, gateway: bool = False) -> Endpoint:
        """See ``vmlayer.Sandbox.expose``."""
        c = self._client._conn
        req = sandboxes_pb2.ExposeRequest(id=self.id, guest_port=port, gateway=gateway)
        return _endpoint(await c.call(c.sandboxes.Expose, req, timeout=120))

    async def unexpose(self, port: int) -> None:
        c = self._client._conn
        req = sandboxes_pb2.UnexposeRequest(id=self.id, guest_port=port)
        await c.call(c.sandboxes.Unexpose, req)

    async def wait_for_port(self, port: int, *, timeout: timedelta = timedelta(seconds=30)) -> None:
        c = self._client._conn
        req = sandboxes_pb2.WaitForPortRequest(
            id=self.id, port=port, timeout=_rpc.to_duration(timeout)
        )
        await c.call(c.sandboxes.WaitForPort, req, timeout=timeout.total_seconds() + 10)

    # -- context manager ------------------------------------------------------

    async def __aenter__(self) -> AsyncSandbox:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.kill()


class AsyncProcess:
    """A command started with ``AsyncSandbox.spawn``.

    ``async for chunk in proc`` streams output from the start (the daemon
    buffers it); ``await proc.wait()`` returns the full result.
    """

    def __init__(self, client: AsyncClient, sandbox_id: str, pid: int) -> None:
        self._client = client
        self._sandbox_id = sandbox_id
        self.pid = pid

    def __repr__(self) -> str:
        return f"AsyncProcess(sandbox={self._sandbox_id!r}, pid={self.pid})"

    async def __aiter__(self) -> AsyncIterator[OutputChunk]:
        c = self._client._conn
        req = processes_pb2.OutputRequest(sandbox_id=self._sandbox_id, pid=self.pid)
        async for ev in c.stream(c.processes.Output, req):
            if ev.WhichOneof("event") == "output":
                yield _chunk(ev.output)

    async def wait(self, timeout: timedelta | None = None) -> ExecResult:
        """Wait for exit and return the output collected since the start."""
        collector = _Collector()
        async for chunk in self:
            collector.add(chunk)
        c = self._client._conn
        req = processes_pb2.WaitRequest(
            sandbox_id=self._sandbox_id, pid=self.pid, timeout=_rpc.to_duration(timeout)
        )
        rpc_timeout = timeout.total_seconds() + 30 if timeout else None
        status = await c.call(c.processes.Wait, req, timeout=rpc_timeout)
        return collector.result(status)

    async def kill(self, signal: int = 9) -> None:
        c = self._client._conn
        req = processes_pb2.KillProcessRequest(
            sandbox_id=self._sandbox_id, pid=self.pid, signal=signal
        )
        await c.call(c.processes.Kill, req)


class AsyncFiles:
    """Filesystem operations inside a sandbox, reached as ``sandbox.files``."""

    def __init__(self, client: AsyncClient, sandbox_id: str) -> None:
        self._client = client
        self._id = sandbox_id

    async def read(self, path: str) -> bytes:
        c = self._client._conn
        req = files_pb2.ReadFileRequest(sandbox_id=self._id, path=path)
        return b"".join([bytes(ch.data) async for ch in c.stream(c.files.Read, req)])

    async def read_text(self, path: str, encoding: str = "utf-8") -> str:
        return (await self.read(path)).decode(encoding)

    async def write(self, path: str, data: bytes | str, *, mode: int = 0o644) -> None:
        """Create or overwrite a file. Parent directories are created."""
        payload = data.encode("utf-8") if isinstance(data, str) else bytes(data)
        header = files_pb2.WriteFileRequest.Header(sandbox_id=self._id, path=path, mode=mode)
        msgs = [files_pb2.WriteFileRequest(header=header)]
        msgs += [
            files_pb2.WriteFileRequest(data=payload[i : i + CHUNK])
            for i in range(0, len(payload), CHUNK)
        ] or [files_pb2.WriteFileRequest(data=b"")]
        c = self._client._conn
        try:
            await c.files.Write(iter(msgs), timeout=c.timeout)
        except grpc.RpcError as e:
            raise _rpc.translate(e) from None

    async def list(self, path: str = "/") -> list[FileInfo]:
        c = self._client._conn
        req = files_pb2.ListFilesRequest(sandbox_id=self._id, path=path)
        return [_info(e) for e in (await c.call(c.files.List, req)).entries]

    async def stat(self, path: str) -> FileInfo:
        c = self._client._conn
        req = files_pb2.StatRequest(sandbox_id=self._id, path=path)
        return _info(await c.call(c.files.Stat, req))

    async def exists(self, path: str) -> bool:
        try:
            await self.stat(path)
            return True
        except NotFound:
            return False

    async def mkdir(self, path: str) -> None:
        c = self._client._conn
        await c.call(c.files.Mkdir, files_pb2.MkdirRequest(sandbox_id=self._id, path=path))

    async def remove(self, path: str, *, recursive: bool = False) -> None:
        c = self._client._conn
        req = files_pb2.RemoveRequest(sandbox_id=self._id, path=path, recursive=recursive)
        await c.call(c.files.Remove, req)

    async def upload(self, local: Path | str, remote: str) -> None:
        """Copy a local file or directory tree into the sandbox."""
        src = Path(local)
        if not src.is_dir():
            data = await asyncio.to_thread(src.read_bytes)
            await self.write(remote, data, mode=src.stat().st_mode & 0o7777)
            return
        await self.mkdir(remote)
        for child in sorted(src.rglob("*")):
            target = f"{remote.rstrip('/')}/{child.relative_to(src).as_posix()}"
            if child.is_dir():
                await self.mkdir(target)
            elif child.is_file():
                data = await asyncio.to_thread(child.read_bytes)
                await self.write(target, data, mode=child.stat().st_mode & 0o7777)

    async def download(self, remote: str, local: Path | str) -> None:
        """Copy a file or directory tree out of the sandbox."""
        dst = Path(local)
        if (await self.stat(remote)).is_dir:
            dst.mkdir(parents=True, exist_ok=True)
            for entry in await self.list(remote):
                await self.download(entry.path, dst / entry.name)
        else:
            dst.parent.mkdir(parents=True, exist_ok=True)
            await asyncio.to_thread(dst.write_bytes, await self.read(remote))
