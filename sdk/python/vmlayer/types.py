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
