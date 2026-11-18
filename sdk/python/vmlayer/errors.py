"""Exceptions raised by the vmlayer SDK. One class per failure the daemon can report."""

from __future__ import annotations

import re
from datetime import timedelta
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .types import ExecResult


class VmlayerError(Exception):
    """Base class for every error raised by this SDK."""


class DaemonUnavailable(VmlayerError):
    """No daemon answers at the endpoint. Is ``vmlayerd`` running?"""


class Unauthenticated(VmlayerError):
    """The daemon did not accept the token (missing, wrong or revoked)."""


class PermissionDenied(VmlayerError):
    """The token's role does not allow this call (e.g. a read-only token
    creating a sandbox)."""


class ResourceExhausted(VmlayerError):
    """The daemon refused because its host is full (admission control), or
    no host in a cluster has room."""


class InvalidArgument(VmlayerError):
    """The daemon rejected the request's arguments."""


class NotFound(VmlayerError):
    """Something other than a sandbox, volume or snapshot was not found (a file, a process)."""


class SandboxNotFound(NotFound):
    """The sandbox id does not exist on the daemon (never created or already gone)."""


class VolumeNotFound(NotFound):
    """No volume with that id or name."""


class SnapshotNotFound(NotFound):
    """No snapshot with that id."""


class VolumeBusy(VmlayerError):
    """The volume is attached read-write elsewhere (single-writer rule)."""


class BackendUnsupported(VmlayerError):
    """The active backend does not implement the requested operation.

    Check ``Sandbox.capabilities`` before calling optional operations if you
    want to avoid this at runtime.
    """

    def __init__(self, message: str, backend: str = "", feature: str = "") -> None:
        super().__init__(message)
        self.backend = backend
        self.feature = feature

    @classmethod
    def from_message(cls, message: str) -> BackendUnsupported:
        m = re.match(r"backend (\S+) does not support (.+)", message)
        if m:
            return cls(message, m.group(1), m.group(2))
        return cls(message)


class TimeoutError(VmlayerError):
    """A daemon-side wait exceeded its timeout (e.g. ``wait_for_port``)."""


class ExecTimeout(TimeoutError):
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
