"""Several daemons that share a blob store (decisions 0014, review A3).

Every daemon started with ``--advertise`` publishes a heartbeat. Placement
happens in the daemons: any one of them picks a host from the shared
heartbeats (``Client.place_sandbox``), and the chosen host admits or refuses
the create by its own limits. This class only follows those answers, so
every SDK language behaves the same::

    cluster = vmlayer.Cluster("tcp://10.0.0.5:7070", token=TOKEN)
    sb = cluster.create("python:3.12", memory_mb=1024, lease="repo-42")
    same = cluster.connect(sb.id)      # from anywhere, later
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from .client import Client
from .errors import NotFound, ResourceExhausted
from .sandbox import Sandbox
from .types import Backend, HostInfo


class NoCapacity(ResourceExhausted):
    """No host in the cluster can take the sandbox."""


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
        cpus: int = 0,
        memory_mb: int = 0,
        placement: Mapping[str, str] | None = None,
        exclude: list[str] | None = None,
    ) -> HostInfo:
        try:
            return self._seed.place_sandbox(
                backend=backend,
                cpus=cpus,
                memory_mb=memory_mb,
                placement=placement,
                exclude_hosts=exclude,
            )
        except ResourceExhausted as e:
            raise NoCapacity(str(e)) from None

    def create(
        self,
        image: str,
        *,
        placement: Mapping[str, str] | None = None,
        backend: Backend = Backend.AUTO,
        cpus: int | None = None,
        memory_mb: int | None = None,
        **kwargs: Any,
    ) -> Sandbox:
        """``Sandbox.create`` on a host the cluster picks. A host that turns
        out to be full (its heartbeat was stale) is skipped and the next one
        tried."""
        refused: list[str] = []
        while True:
            host = self.place(
                backend=backend,
                cpus=cpus or 0,
                memory_mb=memory_mb or 0,
                placement=placement,
                exclude=refused,
            )
            try:
                return Sandbox.create(
                    image,
                    backend=backend,
                    cpus=cpus,
                    memory_mb=memory_mb,
                    client=self.client(host),
                    **kwargs,
                )
            except ResourceExhausted:
                refused.append(host.host_id)

    def connect(self, sandbox_id: str) -> Sandbox:
        """The sandbox with this id, on whichever host runs it."""
        for host in self.hosts():
            if sandbox_id in host.sandbox_ids:
                return Sandbox.connect(sandbox_id, client=self.client(host))
        raise NotFound(f"no host in the registry runs sandbox {sandbox_id}")
