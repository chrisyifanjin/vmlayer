"""An E2B-shaped facade over vmlayer, for code written against the E2B SDK.

Switch an existing program by changing one import::

    # from e2b import Sandbox
    from vmlayer.e2b import Sandbox

    with Sandbox.create("python:3.12-slim", envs={"MODE": "test"}) as sbx:
        result = sbx.commands.run("python3 -c 'print(6 * 7)'")
        sbx.files.write("/tmp/hello.txt", "hi")
        print(result.stdout, sbx.files.read("/tmp/hello.txt"))

The classes, methods, result types and exceptions follow the E2B Python
SDK (``Sandbox.create / connect / list / kill``, ``commands.run`` with
``background=True`` returning a handle, ``files.read / write / list /
exists / remove / make_dir / rename``, ``get_host``). The facade talks to a
vmlayer daemon, not to E2B's API: this is source compatibility, not wire
compatibility.

Mapping of the parts that differ:

* ``template`` is an OCI image reference, or the name of a template made
  with :func:`create_template`: a vmlayer snapshot labelled
  ``e2b.template=<name>``, restored for an instant start. ``"base"`` (the
  E2B default) maps to ``$VMLAYER_E2B_BASE_IMAGE``, default
  ``python:3.12-slim``.
* ``timeout`` is the sandbox's hard lifetime in seconds, as in E2B.
  ``set_timeout``, ``pause`` and metrics have no vmlayer equivalent yet and
  raise :class:`NotImplementedError`.
* ``api_key`` and ``domain`` are accepted and ignored; the daemon endpoint
  comes from ``VMLAYER_ENDPOINT`` or ``client=``.
"""

from __future__ import annotations

import builtins
import os
import shlex
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any, Literal

from . import errors as _errors
from .client import Client
from .process import Process
from .sandbox import Sandbox as _Sandbox
from .types import Backend

TEMPLATE_LABEL = "e2b.template"
METADATA_PREFIX = "e2b.meta."
_DEFAULT_TIMEOUT = 300


class SandboxException(Exception):
    """Base of the facade's errors, like ``e2b.SandboxException``."""


class TimeoutException(SandboxException):
    pass


class NotFoundException(SandboxException):
    pass


@dataclass
class CommandResult:
    stdout: str
    stderr: str
    exit_code: int
    error: str | None = None


class CommandExitException(SandboxException):
    """Raised by ``commands.run`` when the command exits non-zero, as in E2B."""

    def __init__(self, result: CommandResult) -> None:
        super().__init__(f"command exited with code {result.exit_code}: {result.stderr.strip()}")
        self.stdout = result.stdout
        self.stderr = result.stderr
        self.exit_code = result.exit_code
        self.error = result.error


@dataclass
class EntryInfo:
    name: str
    type: Literal["file", "dir"]
    path: str
    size: int = 0
    mode: int = 0
    modified_time: datetime | None = None


@dataclass
class SandboxInfo:
    sandbox_id: str
    template_id: str
    metadata: dict[str, str]
    started_at: datetime
    name: str | None = None


@dataclass
class ProcessInfo:
    pid: int
    cmd: str
    args: list[str] = field(default_factory=list)
    envs: dict[str, str] = field(default_factory=dict)
    cwd: str | None = None
    tag: str | None = None


def _wrap(fn: Callable[[], Any]) -> Any:
    try:
        return fn()
    except _errors.ExecTimeout as e:
        raise TimeoutException(str(e)) from e
    except _errors.TimeoutError as e:
        raise TimeoutException(str(e)) from e
    except _errors.NotFound as e:
        raise NotFoundException(str(e)) from e
    except _errors.VmlayerError as e:
        raise SandboxException(str(e)) from e


def _result(r: Any) -> CommandResult:
    return CommandResult(stdout=r.stdout, stderr=r.stderr, exit_code=r.exit_code)


class CommandHandle:
    """A background command, from ``commands.run(..., background=True)``."""

    def __init__(self, process: Process) -> None:
        self._process = process

    @property
    def pid(self) -> int:
        return self._process.pid

    def wait(
        self,
        on_stdout: Callable[[str], None] | None = None,
        on_stderr: Callable[[str], None] | None = None,
    ) -> CommandResult:
        """Wait for exit; raises ``CommandExitException`` on a non-zero code."""
        if on_stdout or on_stderr:
            for chunk in self._process:
                text = chunk.data.decode(errors="replace")
                if chunk.stream == "stdout" and on_stdout:
                    on_stdout(text)
                elif chunk.stream == "stderr" and on_stderr:
                    on_stderr(text)
        result = _result(_wrap(self._process.wait))
        if result.exit_code != 0:
            raise CommandExitException(result)
        return result

    def kill(self) -> bool:
        _wrap(self._process.kill)
        return True


class Commands:
    def __init__(self, sandbox: _Sandbox) -> None:
        self._sb = sandbox

    def run(
        self,
        cmd: str,
        background: bool = False,
        envs: Mapping[str, str] | None = None,
        user: str | None = None,
        cwd: str | None = None,
        on_stdout: Callable[[str], None] | None = None,
        on_stderr: Callable[[str], None] | None = None,
        timeout: float | None = 60,
        request_timeout: float | None = None,
    ) -> Any:
        """Run ``cmd`` in a shell. Foreground: returns ``CommandResult`` and
        raises ``CommandExitException`` on a non-zero exit. Background:
        returns a ``CommandHandle``."""
        argv = cmd if user is None else f"su -s /bin/sh {shlex.quote(user)} -c {shlex.quote(cmd)}"
        limit = timedelta(seconds=timeout) if timeout else None
        if background or on_stdout or on_stderr:
            process = _wrap(lambda: self._sb.spawn(argv, cwd=cwd, env=envs, timeout=limit))
            handle = CommandHandle(process)
            return handle if background else handle.wait(on_stdout, on_stderr)
        result = _result(_wrap(lambda: self._sb.exec(argv, cwd=cwd, env=envs, timeout=limit)))
        if result.exit_code != 0:
            raise CommandExitException(result)
        return result

    def list(self) -> list[ProcessInfo]:
        return [
            ProcessInfo(pid=p.pid, cmd=p.argv[0] if p.argv else "", args=list(p.argv[1:]))
            for p in _wrap(self._sb.processes)
        ]

    def kill(self, pid: int) -> bool:
        try:
            _wrap(lambda: self._sb.process(pid).kill())
        except NotFoundException:
            return False
        return True

    def connect(self, pid: int) -> CommandHandle:
        return CommandHandle(_wrap(lambda: self._sb.process(pid)))


class Filesystem:
    def __init__(self, sandbox: _Sandbox) -> None:
        self._sb = sandbox

    def read(self, path: str, format: Literal["text", "bytes"] = "text") -> Any:
        data = _wrap(lambda: self._sb.files.read(path))
        return data if format == "bytes" else data.decode()

    def write(self, path: str, data: str | bytes) -> EntryInfo:
        _wrap(lambda: self._sb.files.write(path, data))
        return EntryInfo(name=path.rsplit("/", 1)[-1], type="file", path=path, size=len(data))

    def list(self, path: str, depth: int | None = 1) -> list[EntryInfo]:
        return [
            EntryInfo(
                name=f.name,
                type="dir" if f.is_dir else "file",
                path=f.path,
                size=f.size,
                mode=f.mode,
                modified_time=datetime.fromtimestamp(f.modified_at, timezone.utc),
            )
            for f in _wrap(lambda: self._sb.files.list(path))
        ]

    def exists(self, path: str) -> bool:
        return bool(_wrap(lambda: self._sb.files.exists(path)))

    def remove(self, path: str) -> None:
        _wrap(lambda: self._sb.files.remove(path, recursive=True))

    def make_dir(self, path: str) -> bool:
        if self.exists(path):
            return False
        _wrap(lambda: self._sb.files.mkdir(path, parents=True))
        return True

    def rename(self, old_path: str, new_path: str) -> EntryInfo:
        r = _wrap(lambda: self._sb.exec(["mv", old_path, new_path]))
        if r.exit_code != 0:
            raise SandboxException(r.stderr.strip())
        return EntryInfo(name=new_path.rsplit("/", 1)[-1], type="file", path=new_path)


def _image(template: str | None) -> str:
    if template in (None, "", "base"):
        return os.environ.get("VMLAYER_E2B_BASE_IMAGE", "python:3.12-slim")
    return template


def _template_snapshot(client: Client, name: str | None) -> Any:
    if not name:
        return None
    snaps = client.list_snapshots(labels={TEMPLATE_LABEL: name})
    return max(snaps, key=lambda s: s.created_at) if snaps else None


class Sandbox:
    """E2B's ``Sandbox``, backed by a vmlayer sandbox (``.vmlayer``)."""

    def __init__(self, sandbox: _Sandbox, template_id: str = "") -> None:
        self.vmlayer = sandbox
        self.template_id = template_id
        self.commands = Commands(sandbox)
        self.files = Filesystem(sandbox)

    @property
    def sandbox_id(self) -> str:
        return self.vmlayer.id

    @classmethod
    def create(
        cls,
        template: str | None = None,
        timeout: int | None = None,
        metadata: Mapping[str, str] | None = None,
        envs: Mapping[str, str] | None = None,
        api_key: str | None = None,
        domain: str | None = None,
        request_timeout: float | None = None,
        *,
        backend: Backend = Backend.AUTO,
        client: Client | None = None,
    ) -> Sandbox:
        from .client import default_client

        c = client or default_client()
        lifetime = timedelta(seconds=timeout or _DEFAULT_TIMEOUT)
        labels = {f"{METADATA_PREFIX}{k}": v for k, v in (metadata or {}).items()}
        labels[TEMPLATE_LABEL] = template or "base"
        snapshot = _template_snapshot(c, template)
        if snapshot is not None:
            if envs:
                raise SandboxException("envs cannot be applied to a snapshot template")
            sb = _wrap(
                lambda: _Sandbox.restore(snapshot, timeout=lifetime, labels=labels, client=c)
            )
        else:
            sb = _wrap(
                lambda: _Sandbox.create(
                    _image(template),
                    env=envs,
                    timeout=lifetime,
                    labels=labels,
                    backend=backend,
                    entrypoint=None,
                    workspace=None,
                    client=c,
                )
            )
        return cls(sb, template or "base")

    @classmethod
    def connect(cls, sandbox_id: str, *, client: Client | None = None, **_: Any) -> Sandbox:
        sb = _wrap(lambda: _Sandbox.connect(sandbox_id, client=client))
        return cls(sb, sb.labels.get(TEMPLATE_LABEL, ""))

    @staticmethod
    def list(*, client: Client | None = None, **_: Any) -> builtins.list[SandboxInfo]:
        from .client import default_client

        c = client or default_client()
        out = []
        for sb in _wrap(c.list_sandboxes):
            labels = dict(sb.labels)
            if TEMPLATE_LABEL not in labels:
                continue
            out.append(
                SandboxInfo(
                    sandbox_id=sb.id,
                    template_id=labels[TEMPLATE_LABEL],
                    metadata={
                        k[len(METADATA_PREFIX) :]: v
                        for k, v in labels.items()
                        if k.startswith(METADATA_PREFIX)
                    },
                    started_at=datetime.fromtimestamp(sb.created_at, timezone.utc),
                )
            )
        return out

    def kill(self) -> bool:
        try:
            _wrap(self.vmlayer.kill)
        except NotFoundException:
            return False
        return True

    def is_running(self) -> bool:
        try:
            return bool(_wrap(lambda: self.vmlayer.refresh().state.value) == "running")
        except NotFoundException:
            return False

    def get_host(self, port: int) -> str:
        endpoint = _wrap(lambda: self.vmlayer.expose(port))
        return f"{endpoint.host}:{endpoint.port}"

    def set_timeout(self, timeout: int) -> None:
        raise NotImplementedError("vmlayer cannot change a running sandbox's lifetime yet")

    def pause(self) -> str:
        raise NotImplementedError(
            "use sbx.vmlayer.snapshot() and vmlayer.Sandbox.restore(); a restore gets a new id"
        )

    def get_metrics(self) -> builtins.list[Any]:
        raise NotImplementedError("vmlayer does not report sandbox metrics yet")

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *_: object) -> None:
        self.kill()


def create_template(
    name: str,
    image: str,
    setup: str | None = None,
    *,
    backend: Backend = Backend.AUTO,
    client: Client | None = None,
) -> str:
    """Make an E2B-style template: boot ``image``, run ``setup``, snapshot it
    with the label ``e2b.template=<name>``. ``Sandbox.create(name)`` then
    restores it. On microVM backends the snapshot includes memory, so
    processes started by ``setup`` are running in every new sandbox."""
    from .client import default_client

    c = client or default_client()
    sb = _wrap(
        lambda: _Sandbox.create(
            image, backend=backend, entrypoint=None, workspace=None, timeout=None, client=c
        )
    )
    try:
        if setup:
            r = _wrap(lambda: sb.exec(setup, timeout=timedelta(minutes=30)))
            if r.exit_code != 0:
                raise CommandExitException(_result(r))
        snap = _wrap(lambda: sb.snapshot(labels={TEMPLATE_LABEL: name}))
        return str(snap.id)
    finally:
        sb.kill()
