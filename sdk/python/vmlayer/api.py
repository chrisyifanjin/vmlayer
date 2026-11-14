"""Public API surface of the vmlayer Python SDK.

Design rules, in priority order:

1. The unit is a *sandbox*, never a VM. Callers never see vCPUs, tap devices,
   rootfs paths, or which hypervisor is underneath.
2. Every backend exposes the same interface. A backend that cannot do
   something (e.g. Docker cannot ``fork``) raises ``BackendUnsupported`` and
   reports it in ``Capabilities`` so callers can check up front.
3. Snapshot / restore / fork are first-class, not add-ons. They are the reason
   to use microVMs instead of containers.
4. Stay thin. The SDK is a client to the local ``vmlayerd`` daemon (gRPC over a
   unix socket by default). No business logic lives here.

Everything below is a signature with a docstring. Bodies are
``NotImplementedError`` until step 4 of the roadmap.
"""

from __future__ import annotations

import enum
from collections.abc import AsyncIterator, Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import timedelta
from pathlib import Path
from typing import (
    IO,
    Literal,
)

# --------------------------------------------------------------------------
# Errors
# --------------------------------------------------------------------------


class VmlayerError(Exception):
    """Base class for every error raised by this SDK."""


class SandboxNotFound(VmlayerError):
    """The sandbox id does not exist on the daemon (never created or already gone)."""


class BackendUnsupported(VmlayerError):
    """The active backend does not implement the requested operation.

    Check ``Sandbox.capabilities`` before calling optional operations if you
    want to avoid this at runtime.
    """

    def __init__(self, backend: Backend, feature: str) -> None:
        super().__init__(f"backend {backend.value!r} does not support {feature!r}")
        self.backend = backend
        self.feature = feature


class ExecTimeout(VmlayerError):
    """A command exceeded its ``timeout``. The process has been killed.

    ``partial`` holds whatever the command produced before it was stopped, so
    callers can still show the model what happened.
    """

    def __init__(self, timeout: timedelta, partial: ExecResult) -> None:
        super().__init__(f"command exceeded {timeout}")
        self.timeout = timeout
        self.partial = partial


class SnapshotIncompatible(VmlayerError):
    """A snapshot cannot be restored on this host.

    Typical causes: different CPU model, different guest kernel, snapshot taken
    by a different backend.
    """


# --------------------------------------------------------------------------
# Enums and value types
# --------------------------------------------------------------------------


class Backend(str, enum.Enum):
    """Which isolation engine runs the sandbox.

    ``AUTO`` picks the strongest backend available on the host: Firecracker on
    Linux with KVM, libkrun on macOS, Docker otherwise.
    """

    AUTO = "auto"
    FIRECRACKER = "firecracker"
    LIBKRUN = "libkrun"
    DOCKER = "docker"


class AUTO:
    """Sentinel: let the daemon choose. Used as a default where ``None`` means
    something else (e.g. ``workspace=None`` means *no* workspace)."""


class SandboxState(str, enum.Enum):
    CREATING = "creating"
    RUNNING = "running"
    PAUSED = "paused"
    STOPPED = "stopped"
    FAILED = "failed"


@dataclass(frozen=True)
class Capabilities:
    """What the backend behind a sandbox can do.

    Every field maps to one optional method on ``Sandbox``. Calling a method
    whose capability is ``False`` raises ``BackendUnsupported``.
    """

    snapshot: bool
    fork: bool
    pause: bool
    pty: bool
    expose_port: bool
    memory_snapshot: bool
    """True when snapshots include RAM (microVM backends). False when only the
    filesystem is captured (Docker)."""
    volumes: bool
    """Can attach ``Volume`` objects at creation time."""
    hot_attach: bool
    """Can attach/detach volumes while running. No v1 backend sets this."""


@dataclass(frozen=True)
class ExecResult:
    exit_code: int
    stdout: str
    stderr: str
    output: str
    """stdout and stderr merged in arrival order, the way a terminal shows
    them. Usually what you feed back to the model."""
    duration: timedelta

    @property
    def ok(self) -> bool:
        return self.exit_code == 0


@dataclass(frozen=True)
class OutputChunk:
    """One piece of output from a running process, in arrival order."""

    stream: Literal["stdout", "stderr"]
    data: bytes


@dataclass(frozen=True)
class FileInfo:
    path: str
    size: int
    is_dir: bool
    mode: int
    modified_at: float
    """Unix timestamp, seconds."""


@dataclass(frozen=True)
class PtySize:
    cols: int = 80
    rows: int = 24


DEFAULT_PTY_SIZE = PtySize()


@dataclass(frozen=True)
class Endpoint:
    """A host-reachable address for a port inside the sandbox."""

    host: str
    port: int

    @property
    def url(self) -> str:
        return f"http://{self.host}:{self.port}"


@dataclass(frozen=True)
class Snapshot:
    """A frozen copy of a sandbox: filesystem, and RAM when the backend supports it.

    Snapshots are addressed by ``id`` and outlive the sandbox they came from.
    Restore with ``Sandbox.restore(snapshot)``.
    """

    id: str
    backend: Backend
    created_at: float
    size_bytes: int
    includes_memory: bool
    volumes: Sequence[VolumeSnapshot] = ()
    """Filesystem snapshots of every volume that was attached, keyed to their
    guest paths. ``Sandbox.restore`` clones these; it never reattaches the
    original volumes."""
    labels: Mapping[str, str] = field(default_factory=dict)

    def delete(self) -> None:
        """Remove the snapshot from the daemon's store."""
        raise NotImplementedError


# --------------------------------------------------------------------------
# Volumes
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class VolumeSnapshot:
    """A filesystem-only snapshot of a ``Volume``.

    Cheap (copy-on-write, milliseconds), portable across hosts and backends,
    and the thing to keep long-term. Turn it back into a volume with
    ``Volume.from_snapshot``; use it as a template by doing that many times.
    """

    id: str
    volume_id: str
    guest_path: str | None
    """Where it was mounted when taken, if it was attached."""
    created_at: float
    size_bytes: int
    durable: bool
    """True once the snapshot has been uploaded to the configured blob store."""
    labels: Mapping[str, str] = field(default_factory=dict)

    def delete(self) -> None:
        raise NotImplementedError


class Volume:
    """Persistent storage with a lifetime independent of any sandbox.

    A volume is a block of storage (ext4 inside) that a sandbox mounts at a
    path. It is the workspace primitive: vmlayer does not know what a repo or
    a dependency cache is; put whatever you like on a volume and describe it
    with ``labels``.

    Rules:

    * Read-write attachment to at most one sandbox at a time. Read-only
      attachments are unlimited. A second read-write attach fails.
    * ``Sandbox.restore`` and ``Sandbox.fork`` attach *clones*, never the
      original. The original stays with whoever holds it.
    * Anonymous volumes (created implicitly by ``Sandbox.create``) are deleted
      with their sandbox. Named volumes persist until ``delete``.
    """

    id: str
    name: str | None
    size_mb: int
    labels: Mapping[str, str]
    attached_to: str | None
    """Sandbox id holding the read-write attachment, if any."""

    @classmethod
    def create(
        cls,
        *,
        name: str | None = None,
        size_mb: int | None = None,
        labels: Mapping[str, str] | None = None,
        client: Client | None = None,
    ) -> Volume:
        """Create an empty volume. ``size_mb`` defaults to the daemon's setting.
        Storage is sparse, so a large size costs nothing until used."""
        raise NotImplementedError

    @classmethod
    def from_snapshot(
        cls,
        snapshot: VolumeSnapshot | str,
        *,
        name: str | None = None,
        labels: Mapping[str, str] | None = None,
        client: Client | None = None,
    ) -> Volume:
        """Create a volume whose contents are a copy-on-write clone of a
        snapshot. This is how templates work: snapshot once, instantiate many."""
        raise NotImplementedError

    @classmethod
    def get(cls, name_or_id: str, *, client: Client | None = None) -> Volume:
        raise NotImplementedError

    def snapshot(
        self,
        *,
        durable: bool = False,
        labels: Mapping[str, str] | None = None,
    ) -> VolumeSnapshot:
        """Take a filesystem snapshot.

        Returns as soon as the local copy-on-write snapshot exists. With
        ``durable=True`` it also waits for upload to the blob store. A volume
        attached read-write is quiesced (``fsfreeze`` via the guest agent) for
        the duration, typically milliseconds.
        """
        raise NotImplementedError

    def clone(self, *, name: str | None = None) -> Volume:
        """Copy-on-write copy of the current contents. Shorthand for
        ``snapshot()`` followed by ``from_snapshot``."""
        raise NotImplementedError

    def delete(self) -> None:
        """Fails if the volume is attached."""
        raise NotImplementedError


# --------------------------------------------------------------------------
# Process and PTY handles
# --------------------------------------------------------------------------


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


# --------------------------------------------------------------------------
# Files namespace
# --------------------------------------------------------------------------


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


# --------------------------------------------------------------------------
# Client (daemon connection)
# --------------------------------------------------------------------------


class Client:
    """Connection to a ``vmlayerd`` daemon.

    Most callers never construct one: ``Sandbox.create`` uses a module-level
    default that reads ``VMLAYER_ENDPOINT`` or falls back to the local unix
    socket. Build a ``Client`` explicitly to talk to a remote daemon or to a
    cluster endpoint.
    """

    def __init__(
        self,
        endpoint: str | None = None,
        *,
        token: str | None = None,
        timeout: timedelta = timedelta(seconds=30),
    ) -> None:
        raise NotImplementedError

    def list_sandboxes(self, *, labels: Mapping[str, str] | None = None) -> list[Sandbox]:
        raise NotImplementedError

    def list_snapshots(self, *, labels: Mapping[str, str] | None = None) -> list[Snapshot]:
        raise NotImplementedError

    def list_volumes(self, *, labels: Mapping[str, str] | None = None) -> list[Volume]:
        raise NotImplementedError

    def available_backends(self) -> list[Backend]:
        """Backends the daemon can actually use on this host."""
        raise NotImplementedError

    def close(self) -> None:
        raise NotImplementedError


# --------------------------------------------------------------------------
# Sandbox
# --------------------------------------------------------------------------


class Sandbox:
    """One isolated environment. Create with ``Sandbox.create``, restore with
    ``Sandbox.restore``, attach to an existing one with ``Sandbox.connect``.

    Works as a context manager; leaving the block calls ``kill``.
    """

    id: str
    backend: Backend
    capabilities: Capabilities
    files: Files
    workspace: Volume | None
    """The volume mounted at ``workdir``, or ``None`` if created with
    ``workspace=None``."""
    mounts: Mapping[str, Volume]
    """Every attached volume by guest path, ``workspace`` included."""

    # -- construction -------------------------------------------------------

    @classmethod
    def create(
        cls,
        image: str,
        *,
        cpus: int = 1,
        memory_mb: int = 512,
        disk_mb: int | None = None,
        env: Mapping[str, str] | None = None,
        workdir: str = "/work",
        workspace: Volume | str | None | type[AUTO] = AUTO,
        mounts: Mapping[str, Volume | tuple[Volume, Literal["ro", "rw"]]] | None = None,
        entrypoint: Sequence[str] | None | type[AUTO] = AUTO,
        timeout: timedelta | None = timedelta(minutes=15),
        backend: Backend = Backend.AUTO,
        network: bool = True,
        labels: Mapping[str, str] | None = None,
        client: Client | None = None,
    ) -> Sandbox:
        """Boot a new sandbox from an image.

        ``image`` is an OCI reference (``python:3.12``,
        ``ghcr.io/org/agent-base:v3``). The daemon converts OCI images to the
        backend's rootfs format and caches the result. The root disk is
        disposable: it is the image plus a thin overlay.

        ``workspace`` is the volume mounted at ``workdir``:

        * ``AUTO`` (default): a fresh anonymous volume that is deleted with
          the sandbox. Most callers never touch this.
        * a ``Volume`` or its name/id: reuse persistent storage. Fails if it is
          already attached read-write elsewhere.
        * ``None``: no workspace volume; work on the root overlay only.
          Slightly faster boot, nothing survives the sandbox.

        ``mounts`` attaches additional volumes at other paths, read-write by
        default; use ``(volume, "ro")`` for shared read-only data.

        ``entrypoint`` is a command the guest agent starts as soon as the
        sandbox is up, for images that *are* a server (Jupyter kernel gateway,
        an agent server, a dev server). ``AUTO`` uses the image's
        ``ENTRYPOINT``/``CMD``; ``None`` starts nothing. Its output is
        available via ``process(1)``. Combine with ``wait_for_port`` and
        ``expose``.

        ``timeout`` is a hard wall-clock lifetime; the sandbox is killed when
        it elapses. ``None`` disables it.
        """
        raise NotImplementedError

    @classmethod
    def restore(
        cls,
        snapshot: Snapshot | str,
        *,
        timeout: timedelta | None = timedelta(minutes=15),
        labels: Mapping[str, str] | None = None,
        client: Client | None = None,
    ) -> Sandbox:
        """Start a new sandbox from a snapshot.

        With a memory snapshot this resumes exactly where ``snapshot()`` was
        called, including running processes. Every volume that was attached
        comes back as a fresh copy-on-write clone; the originals are not
        touched. Raises ``SnapshotIncompatible`` if the host cannot run it.
        """
        raise NotImplementedError

    @classmethod
    def connect(cls, sandbox_id: str, *, client: Client | None = None) -> Sandbox:
        """Attach to a sandbox that already exists on the daemon."""
        raise NotImplementedError

    # -- commands -------------------------------------------------------------

    def exec(
        self,
        cmd: str | Sequence[str],
        *,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        stdin: bytes | None = None,
        timeout: timedelta | None = timedelta(minutes=5),
    ) -> ExecResult:
        """Run a command to completion and return its output.

        A ``str`` is run through ``/bin/sh -c``; a sequence is exec'd directly.
        Raises ``ExecTimeout`` if ``timeout`` elapses. A non-zero exit code is
        *not* an exception; check ``result.ok``.
        """
        raise NotImplementedError

    def spawn(
        self,
        cmd: str | Sequence[str],
        *,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        timeout: timedelta | None = None,
    ) -> Process:
        """Start a command and return immediately with a streaming handle."""
        raise NotImplementedError

    def processes(self) -> list[Process]:
        """Every process started through vmlayer that is still running,
        including the entrypoint. Handles survive client reconnects: a new
        ``Sandbox.connect`` can pick up output of a ``spawn`` from before."""
        raise NotImplementedError

    def process(self, pid: int) -> Process:
        """Re-attach to a running process by pid. ``process(1)`` is the
        entrypoint. Raises ``SandboxNotFound`` if no such process."""
        raise NotImplementedError

    def pty(
        self,
        cmd: str | Sequence[str] = "/bin/bash",
        *,
        size: PtySize = DEFAULT_PTY_SIZE,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
    ) -> Pty:
        """Open an interactive terminal. Requires ``capabilities.pty``."""
        raise NotImplementedError

    # -- state ----------------------------------------------------------------

    @property
    def state(self) -> SandboxState:
        raise NotImplementedError

    def snapshot(self, *, labels: Mapping[str, str] | None = None) -> Snapshot:
        """Freeze the sandbox and persist it.

        On microVM backends this captures RAM, the root overlay and every
        attached volume atomically; the sandbox is paused for the duration
        (tens of ms) and then resumed. On Docker only filesystems are
        captured. Requires ``capabilities.snapshot``.

        For a cheap, portable, long-lived checkpoint of just the files, use
        ``sandbox.workspace.snapshot()`` instead.
        """
        raise NotImplementedError

    def fork(self, count: int = 1) -> list[Sandbox]:
        """Clone this sandbox ``count`` times, each copy continuing from the
        current state with its own identity (new id, new network address) and
        its own copy-on-write clones of every attached volume.

        The parent keeps running. Requires ``capabilities.fork``.
        """
        raise NotImplementedError

    def pause(self) -> None:
        """Stop the clock without freeing resources. Requires ``capabilities.pause``."""
        raise NotImplementedError

    def resume(self) -> None:
        raise NotImplementedError

    def kill(self) -> None:
        """Destroy the sandbox and release everything. Idempotent."""
        raise NotImplementedError

    # -- volumes --------------------------------------------------------------

    def attach(self, volume: Volume, path: str, *, read_only: bool = False) -> None:
        """Mount a volume while running. Requires ``capabilities.hot_attach``,
        which no v1 backend has; attach at ``create`` time instead."""
        raise NotImplementedError

    def detach(self, path: str) -> None:
        """Requires ``capabilities.hot_attach``."""
        raise NotImplementedError

    # -- network --------------------------------------------------------------

    def expose(self, port: int) -> Endpoint:
        """Make a guest port reachable from the host. Requires ``capabilities.expose_port``."""
        raise NotImplementedError

    def wait_for_port(self, port: int, *, timeout: timedelta = timedelta(seconds=30)) -> None:
        """Block until something inside the guest listens on ``port``.

        The usual sequence for server-shaped images::

            sb = Sandbox.create("jupyter/kernel-gateway")
            sb.wait_for_port(8888)
            url = sb.expose(8888).url
        """
        raise NotImplementedError

    def unexpose(self, port: int) -> None:
        raise NotImplementedError

    # -- context manager ------------------------------------------------------

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *exc: object) -> None:
        self.kill()


# --------------------------------------------------------------------------
# Async mirror
# --------------------------------------------------------------------------


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
