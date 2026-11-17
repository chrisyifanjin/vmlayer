"""Two daemons sharing one blob store: host registry, placement, leases.

Both daemons run on this machine with their own data directories and a
shared directory as the blob store, standing in for two hosts and S3.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from conftest import _daemon_bin

import vmlayer
from vmlayer import Cluster, Sandbox

ALPINE = "alpine:3.20"


def eventually(check: Callable[[], bool], within: float = 15) -> None:
    deadline = time.time() + within
    while not check():
        assert time.time() < deadline, "condition not met in time"
        time.sleep(0.25)


class Host:
    def __init__(self, root: Path, name: str, store: Path, extra: list[str] | None = None) -> None:
        self.dir = root / name
        self.dir.mkdir()
        sock = self.dir / "d.sock"
        self.endpoint = f"unix://{sock}"
        self.proc = subprocess.Popen(
            [
                str(_daemon_bin()),
                "--listen", self.endpoint,
                "--data-dir", str(self.dir / "data"),
                "--blob-store", f"file://{store}",
                "--host-id", name,
                "--advertise", self.endpoint,
                "--heartbeat-secs", "1",
                "--lease-ttl-secs", "3",
                *(extra or []),
            ],
            stdout=open(self.dir / "daemon.log", "wb"),
            stderr=subprocess.STDOUT,
        )  # fmt: skip
        self.client = vmlayer.Client(self.endpoint)
        eventually(lambda: sock.exists() and self._healthy())

    def _healthy(self) -> bool:
        try:
            return self.client.health()
        except vmlayer.VmlayerError:
            return False

    def stop(self) -> None:
        for sb in self.client.list_sandboxes():
            sb.kill()
        self.proc.terminate()
        self.proc.wait(timeout=15)
        self.client.close()


@pytest.fixture(scope="module")
def hosts(daemon: vmlayer.Client) -> Iterator[tuple[Host, Host, Path]]:
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="cluster-", dir=root))
    store = tmp / "blobs"
    a, b = Host(tmp, "host-a", store), Host(tmp, "host-b", store)
    try:
        yield a, b, store
    finally:
        for h in (a, b):
            if h.proc.poll() is None:
                h.stop()
        if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
            shutil.rmtree(tmp, ignore_errors=True)


def test_registry_placement_and_lookup(hosts: tuple[Host, Host, Path]) -> None:
    a, b, _ = hosts
    cluster = Cluster(a.client)
    eventually(lambda: {h.host_id for h in cluster.hosts()} == {"host-a", "host-b"})
    assert all("docker" in h.backends for h in cluster.hosts())

    with Sandbox.create(ALPINE, client=a.client) as busy:
        eventually(
            lambda: busy.id in next(h for h in cluster.hosts() if h.host_id == "host-a").sandbox_ids
        )
        # host-a now has a sandbox and its memory allocated: the next goes to host-b.
        with cluster.create(ALPINE) as placed:
            assert placed.id in {s.id for s in b.client.list_sandboxes()}
            eventually(
                lambda: (
                    placed.id
                    in next(h for h in cluster.hosts() if h.host_id == "host-b").sandbox_ids
                )
            )
            found = cluster.connect(placed.id)
            assert found.exec("echo here").stdout == "here\n"
    with pytest.raises(vmlayer.NoCapacity):
        cluster.place(placement={"zone": "nowhere"})
    with pytest.raises(vmlayer.NotFound):
        cluster.connect("sb-does-not-exist")


def test_lease_allows_one_sandbox_cluster_wide(hosts: tuple[Host, Host, Path]) -> None:
    a, b, store = hosts
    first = Sandbox.create(ALPINE, lease="job-1", client=a.client)
    with pytest.raises(vmlayer.VmlayerError, match="held by host-a/"):
        Sandbox.create(ALPINE, lease="job-1", client=b.client)
    first.kill()  # releases the lease
    second = Sandbox.create(ALPINE, lease="job-1", client=b.client)

    # Someone else takes the lease (as after this host was cut off): the
    # daemon kills its sandbox instead of letting two run.
    lease = store / "leases" / "job-1.json"
    lease.write_text(json.dumps({"name": "job-1", "holder": "intruder/sb-x", "expires_at": 2**62}))

    def gone() -> bool:
        try:
            second.refresh()
            return False
        except vmlayer.SandboxNotFound:
            return True

    eventually(gone, within=10)
    lease.unlink()


def test_stopped_host_leaves_the_registry(hosts: tuple[Host, Host, Path]) -> None:
    a, b, _ = hosts
    b.stop()
    eventually(lambda: [h.host_id for h in Cluster(a.client).hosts()] == ["host-a"])
