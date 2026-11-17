"""Connection to a ``vmlayerd`` daemon."""

from __future__ import annotations

import threading
from collections.abc import Mapping
from datetime import timedelta
from typing import TYPE_CHECKING

from . import _rpc
from .types import (
    Backend,
    BackendInfo,
    Capabilities,
    GarbageCollection,
    HostInfo,
    ImageInfo,
    SystemInfo,
)
from .v1 import common_pb2, sandboxes_pb2, system_pb2, volumes_pb2

if TYPE_CHECKING:
    from .sandbox import Sandbox
    from .volume import Snapshot, Volume, VolumeSnapshot


def _capabilities(c: common_pb2.Capabilities) -> Capabilities:
    return Capabilities(
        snapshot=c.snapshot,
        fork=c.fork,
        pause=c.pause,
        pty=c.pty,
        expose_port=c.expose_port,
        memory_snapshot=c.memory_snapshot,
        volumes=c.volumes,
        hot_attach=c.hot_attach,
        egress_policy=c.egress_policy,
        io_limits=c.io_limits,
    )


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
        self._conn = _rpc.Connection(endpoint, token, timeout)

    @property
    def endpoint(self) -> str:
        return self._conn.endpoint

    def info(self) -> SystemInfo:
        r = self._conn.call(self._conn.system.Info, common_pb2.Empty())
        return SystemInfo(
            daemon_version=r.daemon_version,
            backends=tuple(
                BackendInfo(
                    name=b.name,
                    engine_version=b.engine_version,
                    capabilities=_capabilities(b.capabilities),
                    unavailable_reason=b.unavailable_reason,
                )
                for b in r.backends
            ),
            default_backend=r.default_backend,
            data_dir=r.data_dir,
            blob_store=r.blob_store or None,
        )

    def health(self) -> bool:
        return bool(self._conn.call(self._conn.system.Health, common_pb2.Empty()).ok)

    def available_backends(self) -> dict[Backend, str]:
        """Backends the daemon can actually use on this host, with the exact
        engine version of each (e.g. ``{Backend.DOCKER: "29.5.2"}``)."""
        out: dict[Backend, str] = {}
        for b in self.info().backends:
            if b.available:
                try:
                    out[Backend(b.name)] = b.engine_version
                except ValueError:
                    continue
        return out

    def pull_image(self, image: str, *, backend: Backend = Backend.AUTO) -> ImageInfo:
        """Resolve an image for a backend ahead of time: pull it and, on
        microVM backends, build its root filesystem. Later creates are fast."""
        r = self._conn.call(
            self._conn.system.PullImage,
            system_pb2.PullImageRequest(image=image, backend=backend.value),
            timeout=1800,
        )
        return ImageInfo(
            image=r.image,
            backend=r.backend,
            rootfs_kind=r.rootfs_kind,
            rootfs_path=r.rootfs_path or None,
            entrypoint=tuple(r.entrypoint),
            workdir=r.workdir or None,
        )

    def list_sandboxes(self, *, labels: Mapping[str, str] | None = None) -> list[Sandbox]:
        from .sandbox import Sandbox

        r = self._conn.call(
            self._conn.sandboxes.List,
            sandboxes_pb2.ListSandboxesRequest(labels=_rpc.labels_arg(labels)),
        )
        return [Sandbox._from_pb(self, s) for s in r.sandboxes]

    def list_snapshots(self, *, labels: Mapping[str, str] | None = None) -> list[Snapshot]:
        from .volume import Snapshot

        r = self._conn.call(
            self._conn.system.ListSnapshots,
            system_pb2.ListSnapshotsRequest(labels=_rpc.labels_arg(labels)),
        )
        return [Snapshot._from_pb(self, s) for s in r.snapshots]

    def list_volumes(self, *, labels: Mapping[str, str] | None = None) -> list[Volume]:
        from .volume import Volume

        r = self._conn.call(
            self._conn.volumes.List, volumes_pb2.ListVolumesRequest(labels=_rpc.labels_arg(labels))
        )
        return [Volume._from_pb(self, v) for v in r.volumes]

    def list_volume_snapshots(
        self, *, labels: Mapping[str, str] | None = None, remote: bool = False
    ) -> list[VolumeSnapshot]:
        """This daemon's volume snapshots, or with ``remote=True`` every
        snapshot in its blob store (from any daemon sharing it)."""
        from .volume import VolumeSnapshot

        r = self._conn.call(
            self._conn.volumes.ListSnapshots,
            volumes_pb2.ListVolumeSnapshotsRequest(labels=_rpc.labels_arg(labels), remote=remote),
            timeout=300,
        )
        return [VolumeSnapshot._from_pb(self, s) for s in r.snapshots]

    def list_hosts(self) -> list[HostInfo]:
        """Daemons with a fresh heartbeat in this daemon's blob store."""
        r = self._conn.call(self._conn.system.ListHosts, common_pb2.Empty(), timeout=60)
        return [
            HostInfo(
                host_id=h.host_id,
                endpoint=h.endpoint,
                daemon_version=h.daemon_version,
                backends=tuple(h.backends),
                cpus=h.cpus,
                memory_mb=h.memory_mib,
                allocated_vcpus=h.allocated_vcpus,
                allocated_memory_mb=h.allocated_memory_mib,
                sandbox_ids=tuple(h.sandbox_ids),
                labels=dict(h.labels),
                updated_at=_rpc.from_timestamp(h.updated_at),
            )
            for h in r.hosts
        ]

    def collect_garbage(self, *, dry_run: bool = False) -> GarbageCollection:
        """Delete blob-store chunks no snapshot references any more.

        Chunks younger than the daemon's grace period are kept, so this is
        safe to run while snapshots are being uploaded."""
        r = self._conn.call(
            self._conn.system.CollectGarbage,
            system_pb2.CollectGarbageRequest(dry_run=dry_run),
            timeout=3600,
        )
        return GarbageCollection(
            snapshots=r.snapshots,
            chunks=r.chunks,
            referenced_chunks=r.referenced_chunks,
            recent_chunks=r.recent_chunks,
            deleted_chunks=r.deleted_chunks,
            deleted_bytes=r.deleted_bytes,
        )

    def close(self) -> None:
        self._conn.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


_default_lock = threading.Lock()
_default_client: Client | None = None


def default_client() -> Client:
    """The process-wide client used when none is passed explicitly."""
    global _default_client
    with _default_lock:
        if _default_client is None:
            _default_client = Client()
        return _default_client


def set_default_client(client: Client | None) -> None:
    global _default_client
    with _default_lock:
        _default_client = client
