"""Placement across several daemons that share a blob store (decision 0014).

Every daemon started with ``--advertise`` publishes a heartbeat; ``Cluster``
reads them through any one daemon, picks a host for each new sandbox, and
finds the host that runs an existing one::

    cluster = vmlayer.Cluster("tcp://10.0.0.5:7070", token=TOKEN)
    sb = cluster.create("python:3.12", memory_mb=1024, lease="repo-42")
    ...
    same = cluster.connect(sb.id)      # from anywhere, later

Placement is client-side: there is no scheduler service to run. A host is
eligible when its heartbeat is fresh, it offers the requested backend, its
labels match ``placement``, and it has room for the requested memory. Among
those it picks the one with the most free memory, then the fewest
sandboxes. Use ``lease=`` to make a workload single-instance cluster-wide.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from .client import Client
from .errors import NotFound, VmlayerError
from .sandbox import Sandbox
from .types import Backend, HostInfo


class NoCapacity(VmlayerError):
    """No host in the registry can take the sandbox."""


class Cluster:
    def __init__(self, seed: Client | str, *, token: str | None = None) -> None:
        """``seed`` is any daemon in the cluster (a ``Client`` or endpoint);
        ``token`` is used for every host reached through the registry."""
        self._token = token
        self._seed = seed if isinstance(seed, Client) else Client(seed, token=token)
        self._clients: dict[str, Client] = {}

    def hosts(self) -> list[HostInfo]:
        return self._seed.list_hosts()

    def client(self, host: HostInfo | str) -> Client:
        endpoint = host if isinstance(host, str) else host.endpoint
        if endpoint == self._seed.endpoint:
            return self._seed
        if endpoint not in self._clients:
            self._clients[endpoint] = Client(endpoint, token=self._token)
        return self._clients[endpoint]

    def place(
        self,
        *,
        backend: Backend = Backend.AUTO,
        memory_mb: int = 512,
        placement: Mapping[str, str] | None = None,
    ) -> HostInfo:
        def fits(h: HostInfo) -> bool:
            if backend != Backend.AUTO and backend.value not in h.backends:
                return False
            if not h.backends:
                return False
            if any(h.labels.get(k) != v for k, v in (placement or {}).items()):
                return False
            return not h.memory_mb or h.memory_mb - h.allocated_memory_mb >= memory_mb

        candidates = [h for h in self.hosts() if fits(h)]
        if not candidates:
            raise NoCapacity(
                f"no host offers backend {backend.value!r} with {memory_mb} MiB free"
                + (f" and labels {dict(placement)}" if placement else "")
            )
        return max(
            candidates,
            key=lambda h: (h.memory_mb - h.allocated_memory_mb, -len(h.sandbox_ids)),
        )

    def create(
        self,
        image: str,
        *,
        placement: Mapping[str, str] | None = None,
        backend: Backend = Backend.AUTO,
        memory_mb: int | None = None,
        **kwargs: Any,
    ) -> Sandbox:
        """``Sandbox.create`` on the host ``place`` picks."""
        host = self.place(backend=backend, memory_mb=memory_mb or 512, placement=placement)
        return Sandbox.create(
            image, backend=backend, memory_mb=memory_mb, client=self.client(host), **kwargs
        )

    def connect(self, sandbox_id: str) -> Sandbox:
        """The sandbox with this id, on whichever host runs it."""
        for host in self.hosts():
            if sandbox_id in host.sandbox_ids:
                return Sandbox.connect(sandbox_id, client=self.client(host))
        raise NotFound(f"no host in the registry runs sandbox {sandbox_id}")
