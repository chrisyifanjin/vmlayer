"""Exceptions raised by the vmlayer SDK. One class per failure the daemon can report."""

from __future__ import annotations

from datetime import timedelta
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .types import Backend, ExecResult


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
