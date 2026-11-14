"""Connection to a ``vmlayerd`` daemon."""

from __future__ import annotations

from collections.abc import Mapping
from datetime import timedelta
from typing import TYPE_CHECKING

from .types import Backend

if TYPE_CHECKING:
    from .sandbox import Sandbox
    from .volume import Snapshot, Volume


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

    def available_backends(self) -> dict[Backend, str]:
        """Backends the daemon can actually use on this host, with the exact
        engine version of each (e.g. ``{Backend.FIRECRACKER: "1.16.1"}``)."""
        raise NotImplementedError

    def close(self) -> None:
        raise NotImplementedError
