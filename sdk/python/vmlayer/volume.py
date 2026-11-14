"""Persistent storage: ``Volume``, ``VolumeSnapshot``, and the sandbox-level ``Snapshot``.

See ADR 0006 for the rules (single writer, cloned on restore, two kinds of snapshot)."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from .types import Backend

if TYPE_CHECKING:
    from .client import Client


@dataclass(frozen=True)
class Snapshot:
    """A frozen copy of a sandbox: filesystem, and RAM when the backend supports it.

    Snapshots are addressed by ``id`` and outlive the sandbox they came from.
    Restore with ``Sandbox.restore(snapshot)``.
    """

    id: str
    backend: Backend
    engine_version: str
    """Exact engine release that produced it, e.g. ``firecracker 1.16.1``.
    Memory snapshots are not portable across releases."""
    cpu_model: str | None
    """Host CPU model for memory snapshots. Restore on a different
    microarchitecture raises ``SnapshotIncompatible``."""
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
