"""Value types shared across the SDK: enums, capabilities, results, sizes."""

from __future__ import annotations

import enum
from dataclasses import dataclass
from datetime import timedelta
from typing import Literal


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
    egress_policy: bool = False
    """Enforces ``egress=`` allowlists (microVM backends)."""
    io_limits: bool = False
    """Enforces ``limits=`` (microVM backends)."""


@dataclass(frozen=True)
class IoLimits:
    """Device rate limits for ``Sandbox.create(limits=...)``. ``None`` means
    unlimited."""

    network_mbps: int | None = None
    """Megabits per second, each direction."""
    disk_mbps: int | None = None
    """Megabytes per second, per disk."""
    disk_iops: int | None = None
    """Operations per second, per disk."""


@dataclass(frozen=True)
class ExecResult:
    exit_code: int
    stdout: str
    stderr: str
    output: str
    """stdout and stderr merged in arrival order, the way a terminal shows
    them. Usually what you feed back to the model."""
    duration: timedelta
    signal: int | None = None
    """Set when the process was killed by a signal; ``exit_code`` is then
    128 + signal, like a shell reports it."""

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

    @property
    def name(self) -> str:
        return self.path.rstrip("/").rsplit("/", 1)[-1]


@dataclass(frozen=True)
class ProcessInfo:
    pid: int
    argv: tuple[str, ...]
    started_at: float
    is_entrypoint: bool


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
class BackendInfo:
    name: str
    engine_version: str
    capabilities: Capabilities
    unavailable_reason: str

    @property
    def available(self) -> bool:
        return not self.unavailable_reason


@dataclass(frozen=True)
class SystemInfo:
    daemon_version: str
    backends: tuple[BackendInfo, ...]
    default_backend: str
    data_dir: str
    blob_store: str | None = None
    """URL of the blob store for durable snapshots, if one is configured."""


@dataclass(frozen=True)
class HostInfo:
    """A daemon in the shared host registry (``Client.list_hosts``)."""

    host_id: str
    endpoint: str
    daemon_version: str
    backends: tuple[str, ...]
    cpus: int
    memory_mb: int
    """0 when the host does not report it."""
    allocated_vcpus: int
    allocated_memory_mb: int
    sandbox_ids: tuple[str, ...]
    labels: dict[str, str]
    updated_at: float
    allocatable_vcpus: int = 0
    """What the host admits in total by its own limits (0 = unlimited)."""
    allocatable_memory_mb: int = 0


@dataclass(frozen=True)
class GarbageCollection:
    """What ``Client.collect_garbage`` found and removed in the blob store."""

    snapshots: int
    chunks: int
    referenced_chunks: int
    recent_chunks: int
    """Unreferenced but within the grace period, so kept."""
    deleted_chunks: int
    deleted_bytes: int


@dataclass(frozen=True)
class ImageInfo:
    """An image resolved for one backend."""

    image: str
    backend: str
    rootfs_kind: str
    """``engine`` (held by the container engine), ``ext4`` or ``dir``."""
    rootfs_path: str | None
    entrypoint: tuple[str, ...]
    workdir: str | None
