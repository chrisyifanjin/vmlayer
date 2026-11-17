"""Persistent storage: ``Volume``, ``VolumeSnapshot``, and the sandbox-level ``Snapshot``.

See ADR 0006 for the rules (single writer, cloned on restore, two kinds of snapshot).
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from . import _rpc
from .types import Backend
from .v1 import common_pb2, system_pb2, volumes_pb2

if TYPE_CHECKING:
    from .client import Client


def _client(c: Client | None) -> Client:
    if c is not None:
        return c
    from .client import default_client

    return default_client()


@dataclass(frozen=True)
class VolumeSnapshot:
    """A filesystem-only snapshot of a ``Volume``.

    Cheap (copy-on-write, milliseconds) and the thing to keep long-term. Turn
    it back into a volume with ``Volume.from_snapshot``; use it as a template
    by doing that many times. A *durable* snapshot is also stored in the
    daemon's blob store (S3, Azure Blob, GCS or a shared directory), where any
    daemon sharing that store can restore it.
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
    local: bool = True
    """A copy is on this daemon's disk. False after ``delete(local_only=True)``
    and for snapshots listed from the blob store that another daemon pushed;
    restoring those downloads them."""
    _client: Client | None = field(default=None, repr=False, compare=False)

    @classmethod
    def _from_pb(cls, client: Client, s: common_pb2.VolumeSnapshot) -> VolumeSnapshot:
        return cls(
            id=s.id,
            volume_id=s.volume_id,
            guest_path=s.guest_path or None,
            created_at=_rpc.from_timestamp(s.created_at),
            size_bytes=s.size_bytes,
            durable=s.durable,
            labels=dict(s.labels),
            local=s.local,
            _client=client,
        )

    def delete(self, *, local_only: bool = False) -> None:
        """Delete the snapshot, including its copy in the blob store.

        With ``local_only=True``, free the local disk space of a durable
        snapshot and keep it in the blob store; ``Volume.from_snapshot`` will
        download it when needed."""
        c = _client(self._client)
        c._conn.call(
            c._conn.volumes.DeleteSnapshot,
            volumes_pb2.DeleteVolumeSnapshotRequest(id=self.id, local_only=local_only),
        )


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
    _client: Client | None = field(default=None, repr=False, compare=False)

    @classmethod
    def _from_pb(cls, client: Client, s: common_pb2.Snapshot) -> Snapshot:
        try:
            backend = Backend(s.backend)
        except ValueError:
            backend = Backend.AUTO
        return cls(
            id=s.id,
            backend=backend,
            engine_version=s.engine_version,
            cpu_model=s.cpu_model or None,
            created_at=_rpc.from_timestamp(s.created_at),
            size_bytes=s.size_bytes,
            includes_memory=s.includes_memory,
            volumes=tuple(VolumeSnapshot._from_pb(client, v) for v in s.volumes),
            labels=dict(s.labels),
            _client=client,
        )

    def delete(self) -> None:
        """Remove the snapshot from the daemon's store."""
        c = _client(self._client)
        c._conn.call(c._conn.system.DeleteSnapshot, system_pb2.DeleteSnapshotRequest(id=self.id))


class Volume:
    """Persistent storage with a lifetime independent of any sandbox.

    A volume is a block of storage that a sandbox mounts at a path. It is the
    workspace primitive: vmlayer does not know what a repo or a dependency
    cache is; put whatever you like on a volume and describe it with
    ``labels``.

    Rules:

    * Read-write attachment to at most one sandbox at a time. Read-only
      attachments are unlimited. A second read-write attach fails.
    * ``Sandbox.restore`` and ``Sandbox.fork`` attach *clones*, never the
      original. The original stays with whoever holds it.
    * Anonymous volumes (created implicitly by ``Sandbox.create``) are deleted
      with their sandbox. Named volumes persist until ``delete``.
    """

    def __init__(self, client: Client, pb: common_pb2.Volume) -> None:
        self._client = client
        self._pb = pb

    @classmethod
    def _from_pb(cls, client: Client, pb: common_pb2.Volume) -> Volume:
        return cls(client, pb)

    # -- attributes ---------------------------------------------------------

    @property
    def id(self) -> str:
        return self._pb.id

    @property
    def name(self) -> str | None:
        return self._pb.name or None

    @property
    def size_mb(self) -> int:
        return int(self._pb.size_mib)

    @property
    def labels(self) -> Mapping[str, str]:
        return dict(self._pb.labels)

    @property
    def anonymous(self) -> bool:
        return self._pb.anonymous

    @property
    def attached_to(self) -> str | None:
        """Sandbox id holding the read-write attachment, if any."""
        self.refresh()
        return self._pb.attached_to or None

    def refresh(self) -> Volume:
        c = self._client._conn
        self._pb = c.call(c.volumes.Get, volumes_pb2.GetVolumeRequest(volume=self.id))
        return self

    def __repr__(self) -> str:
        return f"Volume(id={self.id!r}, name={self.name!r}, size_mb={self.size_mb})"

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Volume) and other.id == self.id

    def __hash__(self) -> int:
        return hash(self.id)

    # -- construction -------------------------------------------------------

    @classmethod
    def create(
        cls,
        *,
        name: str | None = None,
        size_mb: int | None = None,
        labels: Mapping[str, str] | None = None,
        backend: Backend = Backend.AUTO,
        client: Client | None = None,
    ) -> Volume:
        """Create an empty volume. ``size_mb`` defaults to the daemon's setting.
        Storage is sparse, so a large size costs nothing until used.

        ``backend`` decides the on-disk representation (a directory for
        Docker, an ext4 image for microVMs); a volume can only be attached to
        sandboxes of a backend with the same representation."""
        c = _client(client)
        req = volumes_pb2.CreateVolumeRequest(
            name=name or "", labels=_rpc.labels_arg(labels), backend=backend.value
        )
        if size_mb is not None:
            req.size_mib = size_mb
        return cls(c, c._conn.call(c._conn.volumes.Create, req))

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
        snapshot. This is how templates work: snapshot once, instantiate many.

        A durable snapshot without a local copy, including one pushed by
        another daemon that shares the blob store, is downloaded first."""
        c = _client(client)
        sid = snapshot if isinstance(snapshot, str) else snapshot.id
        req = volumes_pb2.VolumeFromSnapshotRequest(
            snapshot_id=sid, name=name or "", labels=_rpc.labels_arg(labels)
        )
        return cls(c, c._conn.call(c._conn.volumes.FromSnapshot, req, timeout=3600))

    @classmethod
    def get(cls, name_or_id: str, *, client: Client | None = None) -> Volume:
        c = _client(client)
        return cls(
            c, c._conn.call(c._conn.volumes.Get, volumes_pb2.GetVolumeRequest(volume=name_or_id))
        )

    # -- operations ---------------------------------------------------------

    def snapshot(
        self,
        *,
        durable: bool = False,
        labels: Mapping[str, str] | None = None,
    ) -> VolumeSnapshot:
        """Take a filesystem snapshot.

        Returns as soon as the local copy-on-write snapshot exists. With
        ``durable=True`` it also uploads it to the daemon's blob store and
        returns when the upload is complete; only blocks that are not already
        in the store are sent. A volume attached read-write is quiesced while
        the local copy is taken, typically milliseconds.

        Raises if ``durable=True`` and the daemon has no blob store or the
        upload fails; no snapshot is kept then.
        """
        c = self._client._conn
        req = volumes_pb2.SnapshotVolumeRequest(
            id=self.id, durable=durable, labels=_rpc.labels_arg(labels)
        )
        return VolumeSnapshot._from_pb(
            self._client, c.call(c.volumes.TakeSnapshot, req, timeout=3600)
        )

    def clone(self, *, name: str | None = None) -> Volume:
        """Copy-on-write copy of the current contents."""
        c = self._client._conn
        return Volume(
            self._client,
            c.call(
                c.volumes.Clone,
                volumes_pb2.CloneVolumeRequest(id=self.id, name=name or ""),
                timeout=600,
            ),
        )

    def delete(self) -> None:
        """Fails if the volume is attached."""
        c = self._client._conn
        c.call(c.volumes.Delete, volumes_pb2.DeleteVolumeRequest(id=self.id))
