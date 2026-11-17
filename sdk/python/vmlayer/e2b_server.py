"""An E2B wire-compatible gateway in front of a vmlayer daemon (decision 0017).

Unmodified E2B SDKs work against it by pointing two addresses at it::

    python -m vmlayer.e2b_server --listen 0.0.0.0:3000 --api-key "$KEY"

    export E2B_API_URL=http://gateway:3000
    export E2B_SANDBOX_URL=http://gateway:3000
    export E2B_API_KEY="$KEY"
    python my_e2b_program.py          # from e2b import Sandbox; unchanged

The gateway speaks E2B's REST control plane (``/v2/sandboxes`` and friends)
and, for the in-sandbox agent ("envd"), its Connect RPC services
``process.Process`` and ``filesystem.Filesystem`` plus the ``/files`` and
``/health`` HTTP routes. The E2B SDK names the target sandbox in the
``E2b-Sandbox-Id`` header of every envd request, so one address serves all
sandboxes: no wildcard DNS or TLS certificate is needed. Put a TLS
terminating proxy in front of it for use across networks.

Everything is translated to vmlayer calls on any backend: a sandbox is a
vmlayer sandbox, a command is a vmlayer process, a file operation is a
vmlayer file call. Templates are OCI image references, or the names of
vmlayer snapshots labelled ``e2b.template=<name>`` (``vmlayer.e2b.create_template``).
``base`` maps to ``--base-image``. Sandbox timeouts, including
``set_timeout``, are enforced by the gateway.

Not supported, and answered with an "unimplemented" error rather than an
approximation: pause and resume, filesystem watchers, metrics and logs,
snapshots and forks through the E2B API, and templates built by E2B's
build service.

Needs ``pip install "vmlayer[e2b-server]"`` (the E2B SDK provides the
generated protocol code, so the gateway follows E2B's own definitions).
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import hmac
import os
import secrets
import signal as signals
import threading
import time
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any

from connectrpc.code import Code
from connectrpc.errors import ConnectError
from e2b.envd.filesystem import filesystem_pb as fs_pb
from e2b.envd.filesystem.filesystem_connect import FilesystemASGIApplication
from e2b.envd.process import process_pb as proc_pb
from e2b.envd.process.process_connect import ProcessASGIApplication
from protobuf import Oneof
from starlette.applications import Starlette
from starlette.datastructures import UploadFile
from starlette.requests import Request
from starlette.responses import JSONResponse, Response
from starlette.routing import Route
from starlette.types import Receive, Scope, Send

import vmlayer
from vmlayer import Sandbox
from vmlayer.process import Process

#: envd version the gateway reports: the newest feature set it implements
#: (stdin, default user, close-stdin, octet-stream uploads).
ENVD_VERSION = "0.5.7"
TEMPLATE_LABEL = "e2b.template"
TOKEN_LABEL = "e2b.token"
META_PREFIX = "e2b.meta."
DEFAULT_TIMEOUT = 300
MAX_TIMEOUT = 24 * 3600


def _rfc3339(t: float) -> str:
    return datetime.fromtimestamp(t, timezone.utc).isoformat().replace("+00:00", "Z")


def _error(status: int, message: str) -> JSONResponse:
    return JSONResponse({"code": status, "message": message}, status_code=status)


@dataclass
class Entry:
    """What the gateway tracks per sandbox."""

    sandbox: Sandbox
    template: str
    token: str
    started_at: float
    end_at: float
    metadata: dict[str, str]
    processes: dict[int, Process] = field(default_factory=dict)
    tags: dict[str, int] = field(default_factory=dict)
    shell: str | None = None


class Gateway:
    def __init__(self, client: vmlayer.Client, *, api_key: str | None, base_image: str) -> None:
        self.client = client
        self.api_key = api_key
        self.base_image = base_image
        self.entries: dict[str, Entry] = {}
        self.lock = threading.Lock()
        self._adopt_existing()

    # -- state ------------------------------------------------------------

    def _adopt_existing(self) -> None:
        """After a gateway restart, keep serving the sandboxes it created."""
        for sb in self.client.list_sandboxes():
            labels = dict(sb.labels)
            if TEMPLATE_LABEL not in labels:
                continue
            started = sb.created_at
            end = float(labels.get("e2b.end_at", started + DEFAULT_TIMEOUT))
            self.entries[sb.id] = Entry(
                sandbox=sb,
                template=labels[TEMPLATE_LABEL],
                token=labels.get(TOKEN_LABEL, ""),
                started_at=started,
                end_at=end,
                metadata={
                    k[len(META_PREFIX) :]: v for k, v in labels.items() if k.startswith(META_PREFIX)
                },
            )

    def entry(self, sandbox_id: str) -> Entry:
        with self.lock:
            e = self.entries.get(sandbox_id)
        if e is None:
            raise KeyError(sandbox_id)
        return e

    async def reap(self) -> None:
        """Kill sandboxes whose E2B timeout passed."""
        while True:
            await asyncio.sleep(1)
            now = time.time()
            with self.lock:
                expired = [i for i, e in self.entries.items() if e.end_at <= now]
            for sandbox_id in expired:
                await self.kill(sandbox_id)

    async def kill(self, sandbox_id: str) -> bool:
        with self.lock:
            e = self.entries.pop(sandbox_id, None)
        if e is None:
            return False
        with contextlib.suppress(vmlayer.VmlayerError):
            await asyncio.to_thread(e.sandbox.kill)
        return True

    # -- control plane ----------------------------------------------------

    def _template_snapshot(self, name: str) -> Any:
        snaps = self.client.list_snapshots(labels={TEMPLATE_LABEL: name})
        return max(snaps, key=lambda s: s.created_at) if snaps else None

    async def create(self, body: dict[str, Any]) -> tuple[Entry, str]:
        template = body.get("templateID") or "base"
        timeout = min(int(body.get("timeout") or DEFAULT_TIMEOUT), MAX_TIMEOUT)
        metadata = {str(k): str(v) for k, v in (body.get("metadata") or {}).items()}
        env = {str(k): str(v) for k, v in (body.get("envVars") or {}).items()}
        network = body.get("allow_internet_access", True) is not False
        token = secrets.token_urlsafe(24)
        now = time.time()
        labels = {f"{META_PREFIX}{k}": v for k, v in metadata.items()}
        labels.update(
            {TEMPLATE_LABEL: template, TOKEN_LABEL: token, "e2b.end_at": str(now + timeout)}
        )
        # vmlayer's own TTL is the backstop; the gateway enforces the real one.
        hard = timedelta(seconds=MAX_TIMEOUT)
        snapshot = await asyncio.to_thread(self._template_snapshot, template)
        if snapshot is not None:
            sb = await asyncio.to_thread(
                Sandbox.restore, snapshot, timeout=hard, labels=labels, client=self.client
            )
        else:
            image = self.base_image if template == "base" else template
            sb = await asyncio.to_thread(
                Sandbox.create,
                image,
                env=env,
                timeout=hard,
                labels=labels,
                network=network,
                entrypoint=None,
                workspace=None,
                client=self.client,
            )
        e = Entry(sb, template, token, now, now + timeout, metadata)
        with self.lock:
            self.entries[sb.id] = e
        return e, template

    def sandbox_json(self, sandbox_id: str, e: Entry, *, detail: bool = False) -> dict[str, Any]:
        out: dict[str, Any] = {
            "templateID": e.template,
            "sandboxID": sandbox_id,
            "clientID": "vmlayer",
            "envdVersion": ENVD_VERSION,
            "envdAccessToken": e.token,
        }
        if detail:
            out.update(
                {
                    "startedAt": _rfc3339(e.started_at),
                    "endAt": _rfc3339(e.end_at),
                    "cpuCount": 1,
                    "memoryMB": 512,
                    "diskSizeMB": 0,
                    "state": "running",
                    "metadata": e.metadata,
                }
            )
        return out

    # -- envd helpers -----------------------------------------------------

    async def argv(self, e: Entry, config: Any) -> list[str]:
        """E2B runs commands as ``/bin/bash -l -c CMD``; images without bash
        get ``/bin/sh -c CMD`` instead."""
        argv = [config.cmd, *list(config.args)]
        if config.cmd in ("/bin/bash", "bash"):
            if e.shell is None:
                has_bash = await asyncio.to_thread(e.sandbox.files.exists, "/bin/bash")
                e.shell = "/bin/bash" if has_bash else "/bin/sh"
            if e.shell == "/bin/sh":
                rest = [a for a in argv[1:] if a not in ("-l", "--login")]
                argv = ["/bin/sh", *rest]
        return argv


def _sandbox_id(ctx: Any) -> str:
    sid = ctx.request_headers.get("e2b-sandbox-id")
    if not sid:
        raise ConnectError(Code.INVALID_ARGUMENT, "missing E2b-Sandbox-Id header")
    return str(sid)


def _event(kind: str, value: Any) -> proc_pb.ProcessEvent:
    return proc_pb.ProcessEvent(event=Oneof(kind, value))


def _vmlayer_error(e: Exception) -> ConnectError:
    if isinstance(e, vmlayer.NotFound | KeyError):
        return ConnectError(Code.NOT_FOUND, str(e))
    if isinstance(e, ConnectError):
        return e
    return ConnectError(Code.INTERNAL, str(e))


class ProcessService:
    def __init__(self, gw: Gateway) -> None:
        self.gw = gw

    def _entry(self, ctx: Any) -> Entry:
        try:
            return self.gw.entry(_sandbox_id(ctx))
        except KeyError as e:
            raise ConnectError(Code.NOT_FOUND, f"sandbox {e} not found") from None

    def _pid(self, e: Entry, selector: Any) -> int:
        match selector.selector if selector else None:
            case Oneof(field="pid", value=pid):
                return int(pid)
            case Oneof(field="tag", value=tag):
                if tag in e.tags:
                    return e.tags[tag]
        raise ConnectError(Code.NOT_FOUND, "process not found")

    async def _stream(self, proc: Process) -> AsyncIterator[proc_pb.ProcessEvent]:
        """Forward a vmlayer process's output as E2B process events."""
        loop = asyncio.get_running_loop()
        queue: asyncio.Queue[Any] = asyncio.Queue()

        def pump() -> None:
            try:
                for chunk in proc:
                    loop.call_soon_threadsafe(queue.put_nowait, chunk)
                result = proc.wait()
                loop.call_soon_threadsafe(queue.put_nowait, result)
            except Exception as err:
                loop.call_soon_threadsafe(queue.put_nowait, err)

        threading.Thread(target=pump, daemon=True).start()
        while True:
            item = await queue.get()
            if isinstance(item, Exception):
                raise _vmlayer_error(item)
            if isinstance(item, vmlayer.ExecResult):
                end = proc_pb.ProcessEvent.EndEvent(
                    exit_code=item.exit_code,
                    exited=item.signal is None,
                    status=f"exit status {item.exit_code}"
                    if item.signal is None
                    else f"signal: {item.signal}",
                )
                yield _event("end", end)
                return
            data = proc_pb.ProcessEvent.DataEvent(output=Oneof(item.stream, item.data))
            yield _event("data", data)

    async def start(self, request: Any, ctx: Any) -> AsyncIterator[Any]:
        e = self._entry(ctx)
        if request.pty is not None:
            raise ConnectError(Code.UNIMPLEMENTED, "PTY sessions are not supported by the gateway")
        config = request.process
        argv = await self.gw.argv(e, config)
        try:
            proc = await asyncio.to_thread(
                e.sandbox.spawn, argv, cwd=config.cwd or None, env=dict(config.envs) or None
            )
        except vmlayer.VmlayerError as err:
            raise _vmlayer_error(err) from None
        e.processes[proc.pid] = proc
        if request.tag:
            e.tags[request.tag] = proc.pid
        yield proc_pb.StartResponse(
            event=_event("start", proc_pb.ProcessEvent.StartEvent(pid=proc.pid))
        )
        try:
            async for ev in self._stream(proc):
                yield proc_pb.StartResponse(event=ev)
        finally:
            e.processes.pop(proc.pid, None)
            if request.tag:
                e.tags.pop(request.tag, None)

    async def connect(self, request: Any, ctx: Any) -> AsyncIterator[Any]:
        e = self._entry(ctx)
        pid = self._pid(e, request.process)
        try:
            proc = e.processes.get(pid)
            if proc is None:
                proc = await asyncio.to_thread(e.sandbox.process, pid)
        except vmlayer.VmlayerError as err:
            raise _vmlayer_error(err) from None
        yield proc_pb.ConnectResponse(
            event=_event("start", proc_pb.ProcessEvent.StartEvent(pid=pid))
        )
        async for ev in self._stream(proc):
            yield proc_pb.ConnectResponse(event=ev)

    async def list(self, request: Any, ctx: Any) -> Any:
        e = self._entry(ctx)
        tags = {pid: tag for tag, pid in e.tags.items()}
        infos = await asyncio.to_thread(e.sandbox.processes)
        return proc_pb.ListResponse(
            processes=[
                proc_pb.ProcessInfo(
                    config=proc_pb.ProcessConfig(
                        cmd=p.argv[0] if p.argv else "", args=list(p.argv[1:])
                    ),
                    pid=p.pid,
                    tag=tags.get(p.pid),
                )
                for p in infos
            ]
        )

    async def send_signal(self, request: Any, ctx: Any) -> Any:
        e = self._entry(ctx)
        pid = self._pid(e, request.process)
        number = 15 if "SIGTERM" in str(request.signal) else 9
        try:
            proc = e.processes.get(pid)
            if proc is None:
                proc = await asyncio.to_thread(e.sandbox.process, pid)
            await asyncio.to_thread(proc.kill, number)
        except vmlayer.VmlayerError as err:
            raise _vmlayer_error(err) from None
        return proc_pb.SendSignalResponse()

    async def send_input(self, request: Any, ctx: Any) -> Any:
        e = self._entry(ctx)
        pid = self._pid(e, request.process)
        proc = e.processes.get(pid)
        if proc is None:
            raise ConnectError(Code.NOT_FOUND, f"process {pid} has no open stdin here")
        match request.input.input if request.input else None:
            case Oneof(field="stdin", value=data):
                await asyncio.to_thread(proc.stdin.write, bytes(data))
                await asyncio.to_thread(proc.stdin.flush)
            case _:
                raise ConnectError(Code.UNIMPLEMENTED, "PTY input is not supported")
        return proc_pb.SendInputResponse()

    async def close_stdin(self, request: Any, ctx: Any) -> Any:
        e = self._entry(ctx)
        proc = e.processes.get(self._pid(e, request.process))
        if proc is not None:
            await asyncio.to_thread(proc.stdin.close)
        return proc_pb.CloseStdinResponse()

    async def stream_input(self, request: Any, ctx: Any) -> Any:
        raise ConnectError(Code.UNIMPLEMENTED, "streamed input is not supported; use send_input")

    async def update(self, request: Any, ctx: Any) -> Any:
        raise ConnectError(Code.UNIMPLEMENTED, "PTY resize is not supported")


def _entry_info(f: vmlayer.FileInfo) -> fs_pb.EntryInfo:
    kind = fs_pb.FileType.DIRECTORY if f.is_dir else fs_pb.FileType.FILE
    return fs_pb.EntryInfo(
        name=f.name,
        type=kind,
        path=f.path,
        size=f.size,
        mode=f.mode,
        permissions=_permissions(f.mode, f.is_dir),
    )


def _permissions(mode: int, is_dir: bool) -> str:
    bits = "".join(c if mode & (1 << (8 - i)) else "-" for i, c in enumerate("rwxrwxrwx"))
    return ("d" if is_dir else "-") + bits


class FilesystemService:
    def __init__(self, gw: Gateway) -> None:
        self.gw = gw

    def _sb(self, ctx: Any) -> Sandbox:
        try:
            return self.gw.entry(_sandbox_id(ctx)).sandbox
        except KeyError as e:
            raise ConnectError(Code.NOT_FOUND, f"sandbox {e} not found") from None

    async def _call(self, fn: Callable[..., Any], *args: Any, **kw: Any) -> Any:
        try:
            return await asyncio.to_thread(fn, *args, **kw)
        except vmlayer.VmlayerError as err:
            raise _vmlayer_error(err) from None

    async def stat(self, request: Any, ctx: Any) -> Any:
        sb = self._sb(ctx)
        return fs_pb.StatResponse(entry=_entry_info(await self._call(sb.files.stat, request.path)))

    async def list_dir(self, request: Any, ctx: Any) -> Any:
        sb = self._sb(ctx)
        depth = max(int(request.depth or 1), 1)
        entries: list[fs_pb.EntryInfo] = []
        frontier = [(request.path, 1)]
        while frontier:
            path, level = frontier.pop()
            for f in await self._call(sb.files.list, path):
                entries.append(_entry_info(f))
                if f.is_dir and level < depth:
                    frontier.append((f.path, level + 1))
        return fs_pb.ListDirResponse(entries=entries)

    async def make_dir(self, request: Any, ctx: Any) -> Any:
        sb = self._sb(ctx)
        if await self._call(sb.files.exists, request.path):
            raise ConnectError(Code.ALREADY_EXISTS, f"{request.path} already exists")
        await self._call(sb.files.mkdir, request.path, parents=True)
        return fs_pb.MakeDirResponse(
            entry=_entry_info(await self._call(sb.files.stat, request.path))
        )

    async def move(self, request: Any, ctx: Any) -> Any:
        sb = self._sb(ctx)
        r = await self._call(sb.exec, ["mv", request.source, request.destination])
        if r.exit_code != 0:
            code = Code.NOT_FOUND if "No such file" in r.stderr else Code.INTERNAL
            raise ConnectError(code, r.stderr.strip())
        return fs_pb.MoveResponse(
            entry=_entry_info(await self._call(sb.files.stat, request.destination))
        )

    async def remove(self, request: Any, ctx: Any) -> Any:
        sb = self._sb(ctx)
        if not await self._call(sb.files.exists, request.path):
            raise ConnectError(Code.NOT_FOUND, f"{request.path} not found")
        await self._call(sb.files.remove, request.path, recursive=True)
        return fs_pb.RemoveResponse()

    def watch_dir(self, request: Any, ctx: Any) -> AsyncIterator[Any]:
        raise ConnectError(Code.UNIMPLEMENTED, "filesystem watchers are not supported")

    async def create_watcher(self, request: Any, ctx: Any) -> Any:
        raise ConnectError(Code.UNIMPLEMENTED, "filesystem watchers are not supported")

    async def get_watcher_events(self, request: Any, ctx: Any) -> Any:
        raise ConnectError(Code.UNIMPLEMENTED, "filesystem watchers are not supported")

    async def remove_watcher(self, request: Any, ctx: Any) -> Any:
        raise ConnectError(Code.UNIMPLEMENTED, "filesystem watchers are not supported")


def build_app(gw: Gateway) -> Callable[[Scope, Receive, Send], Awaitable[None]]:
    """The ASGI application: REST, envd HTTP routes, and the Connect services."""

    async def create(request: Request) -> Response:
        body = await request.json()
        try:
            e, _ = await gw.create(body)
        except vmlayer.VmlayerError as err:
            status = 404 if isinstance(err, vmlayer.NotFound) else 400
            return _error(status, str(err))
        return JSONResponse(gw.sandbox_json(e.sandbox.id, e), status_code=201)

    def lookup(request: Request) -> tuple[str, Entry] | Response:
        sid = request.path_params["sandbox_id"]
        try:
            return sid, gw.entry(sid)
        except KeyError:
            return _error(404, f"sandbox {sid} not found")

    async def get_one(request: Request) -> Response:
        found = lookup(request)
        if isinstance(found, Response):
            return found
        return JSONResponse(gw.sandbox_json(*found, detail=True))

    async def connect(request: Request) -> Response:
        found = lookup(request)
        if isinstance(found, Response):
            return found
        sid, e = found
        with contextlib.suppress(Exception):
            body = await request.json()
            if body.get("timeout"):
                e.end_at = time.time() + min(int(body["timeout"]), MAX_TIMEOUT)
        return JSONResponse(gw.sandbox_json(sid, e), status_code=201)

    async def delete(request: Request) -> Response:
        sid = request.path_params["sandbox_id"]
        if not await gw.kill(sid):
            return _error(404, f"sandbox {sid} not found")
        return Response(status_code=204)

    async def set_timeout(request: Request) -> Response:
        found = lookup(request)
        if isinstance(found, Response):
            return found
        body = await request.json()
        found[1].end_at = time.time() + min(int(body.get("timeout", DEFAULT_TIMEOUT)), MAX_TIMEOUT)
        return Response(status_code=204)

    async def list_all(request: Request) -> Response:
        wanted: dict[str, str] = {}
        for pair in request.query_params.get("metadata", "").split("&"):
            if "=" in pair:
                k, v = pair.split("=", 1)
                wanted[k] = v
        with gw.lock:
            items = list(gw.entries.items())
        return JSONResponse(
            [
                gw.sandbox_json(sid, e, detail=True)
                for sid, e in items
                if all(e.metadata.get(k) == v for k, v in wanted.items())
            ]
        )

    async def unsupported(request: Request) -> Response:
        return _error(501, f"{request.method} {request.url.path} is not supported by vmlayer")

    def envd_sandbox(request: Request) -> Entry | Response:
        sid = request.headers.get("e2b-sandbox-id", "")
        try:
            return gw.entry(sid)
        except KeyError:
            return _error(404, f"sandbox {sid} not found")

    async def health(request: Request) -> Response:
        sid = request.headers.get("e2b-sandbox-id")
        if sid is None:
            return Response(status_code=204)  # the gateway itself
        try:
            e = gw.entry(sid)
            await asyncio.to_thread(e.sandbox.refresh)
        except (KeyError, vmlayer.VmlayerError):
            # What E2B's edge answers for a sandbox that is gone.
            return _error(502, f"sandbox {sid} is not running")
        return Response(status_code=204)

    async def read_file(request: Request) -> Response:
        e = envd_sandbox(request)
        if isinstance(e, Response):
            return e
        path = request.query_params.get("path", "")
        try:
            data = await asyncio.to_thread(e.sandbox.files.read, path)
        except vmlayer.NotFound:
            return _error(404, f"path '{path}' does not exist")
        except vmlayer.VmlayerError as err:
            return _error(400, str(err))
        return Response(data, media_type="application/octet-stream")

    async def write_file(request: Request) -> Response:
        e = envd_sandbox(request)
        if isinstance(e, Response):
            return e
        written: list[tuple[str, bytes]] = []
        if request.headers.get("content-type", "").startswith("multipart/form-data"):
            form = await request.form()
            for _, part in form.multi_items():
                if isinstance(part, UploadFile):
                    target = request.query_params.get("path") if len(form) == 1 else None
                    written.append((target or part.filename or "", await part.read()))
        else:
            data = await request.body()
            if request.headers.get("content-encoding") == "gzip":
                import gzip

                data = gzip.decompress(data)
            written.append((request.query_params.get("path", ""), data))
        out = []
        for path, data in written:
            if not path:
                return _error(400, "missing file path")
            parent = path.rsplit("/", 1)[0]
            try:
                if parent:
                    await asyncio.to_thread(e.sandbox.files.mkdir, parent, parents=True)
                await asyncio.to_thread(e.sandbox.files.write, path, data)
            except vmlayer.VmlayerError as err:
                return _error(400, str(err))
            out.append({"name": path.rsplit("/", 1)[-1], "type": "file", "path": path})
        return JSONResponse(out)

    rest = Starlette(
        routes=[
            Route("/sandboxes", create, methods=["POST"]),
            Route("/v2/sandboxes", create, methods=["POST"]),
            Route("/sandboxes", list_all, methods=["GET"]),
            Route("/v2/sandboxes", list_all, methods=["GET"]),
            Route("/sandboxes/{sandbox_id}", get_one, methods=["GET"]),
            Route("/sandboxes/{sandbox_id}", delete, methods=["DELETE"]),
            Route("/sandboxes/{sandbox_id}/timeout", set_timeout, methods=["POST"]),
            Route("/sandboxes/{sandbox_id}/connect", connect, methods=["POST"]),
            Route("/v2/sandboxes/{sandbox_id}/connect", connect, methods=["POST"]),
            Route(
                "/sandboxes/{sandbox_id}/{rest:path}", unsupported, methods=["GET", "POST", "PUT"]
            ),
            Route("/health", health, methods=["GET"]),
            Route("/files", read_file, methods=["GET"]),
            Route("/files", write_file, methods=["POST"]),
        ]
    )
    process_app = ProcessASGIApplication(ProcessService(gw))
    fs_app = FilesystemASGIApplication(FilesystemService(gw))

    async def app(scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await rest(scope, receive, send)
            return
        headers = {k.decode().lower(): v.decode() for k, v in scope.get("headers", [])}
        path: str = scope["path"]
        envd = "e2b-sandbox-id" in headers
        if envd:
            # The per-sandbox token the create call handed out.
            sid = headers["e2b-sandbox-id"]
            e = gw.entries.get(sid)
            token = headers.get("x-access-token", "")
            if e is not None and e.token and not hmac.compare_digest(token, e.token):
                await _error(401, "invalid sandbox access token")(scope, receive, send)
                return
        elif gw.api_key and path != "/health":
            if not hmac.compare_digest(headers.get("x-api-key", ""), gw.api_key):
                await _error(401, "invalid API key")(scope, receive, send)
                return
        if path.startswith("/process.Process/"):
            await process_app(scope, receive, send)
        elif path.startswith("/filesystem.Filesystem/"):
            await fs_app(scope, receive, send)
        else:
            await rest(scope, receive, send)

    return app


def main(argv: list[str] | None = None) -> None:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--listen", default=os.environ.get("VMLAYER_E2B_LISTEN", "127.0.0.1:3000"))
    ap.add_argument("--endpoint", default=os.environ.get("VMLAYER_ENDPOINT"), help="vmlayer daemon")
    ap.add_argument("--token", default=os.environ.get("VMLAYER_TOKEN"), help="vmlayer daemon token")
    ap.add_argument(
        "--api-key",
        default=os.environ.get("VMLAYER_E2B_API_KEY"),
        help="required X-API-Key for the control plane (recommended)",
    )
    ap.add_argument(
        "--base-image",
        default=os.environ.get("VMLAYER_E2B_BASE_IMAGE", "python:3.12-slim"),
        help="image for E2B's default 'base' template",
    )
    args = ap.parse_args(argv)
    import uvicorn

    host, _, port = args.listen.rpartition(":")
    client = vmlayer.Client(args.endpoint, token=args.token)
    gw = Gateway(client, api_key=args.api_key, base_image=args.base_image)
    app = build_app(gw)
    config = uvicorn.Config(app, host=host or "127.0.0.1", port=int(port), log_level="info")
    server = uvicorn.Server(config)

    async def run() -> None:
        reaper = asyncio.create_task(gw.reap())
        try:
            await server.serve()
        finally:
            reaper.cancel()

    with contextlib.suppress(KeyboardInterrupt):
        signals.signal(signals.SIGTERM, lambda *_: setattr(server, "should_exit", True))
        asyncio.run(run())


if __name__ == "__main__":
    main()


__all__ = ["ENVD_VERSION", "Gateway", "build_app", "main"]
