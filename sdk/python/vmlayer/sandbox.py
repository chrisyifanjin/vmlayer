"""``Sandbox``: the unit of isolation and the entry point of the SDK."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from datetime import timedelta
from typing import Literal

from .client import Client
from .files import Files
from .process import Process, Pty
from .types import (
    AUTO,
    DEFAULT_PTY_SIZE,
    Backend,
    Capabilities,
    Endpoint,
    ExecResult,
    PtySize,
    SandboxState,
)
from .volume import Snapshot, Volume


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
        workspace: Volume | str | type[AUTO] | None = AUTO,
        mounts: Mapping[str, Volume | tuple[Volume, Literal["ro", "rw"]]] | None = None,
        entrypoint: Sequence[str] | type[AUTO] | None = AUTO,
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
