"""Two daemons sharing one blob store: host registry, placement, leases.

Both daemons run on this machine with their own data directories. The blob
store is a shared directory, or with ``VMLAYER_TEST_BLOB_STORE`` (for
example ``s3://vmlayer-test/cluster`` with the ``AWS_*`` settings from
``scripts/blob-test-stores.sh``) a fresh prefix in object storage, where
leases and ownership records use real conditional writes.
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
from typing import Any

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


class Store:
    """The blob store test daemons share."""

    def __init__(self, tmp: Path) -> None:
        base = os.environ.get("VMLAYER_TEST_BLOB_STORE")
        self.dir = tmp / "blobs"
        self.url = f"{base.rstrip('/')}/{tmp.name}" if base else f"file://{self.dir}"

    def read_json(self, key: str) -> Any:
        """A document in the store, or ``None`` if this test cannot read it."""
        if self.url.startswith("file://"):
            return json.loads((self.dir / key).read_text())
        if self.url.startswith("s3://") and shutil.which("aws"):
            endpoint = os.environ.get("AWS_ENDPOINT")
            out = subprocess.run(
                ["aws", "s3", "cp", f"{self.url}/{key}", "-",
                 *(["--endpoint-url", endpoint] if endpoint else [])],
                capture_output=True, check=True,
            )  # fmt: skip
            return json.loads(out.stdout)
        return None


class Host:
    def __init__(self, root: Path, name: str, store: Store, extra: list[str] | None = None) -> None:
        self.dir = root / name
        self.dir.mkdir()
        sock = self.dir / "d.sock"
        self.endpoint = f"unix://{sock}"
        self.args = [
                str(_daemon_bin()),
                "--listen", self.endpoint,
                "--data-dir", str(self.dir / "data"),
                "--blob-store", store.url,
                "--host-id", name,
                "--advertise", self.endpoint,
                "--heartbeat-secs", "1",
                "--lease-ttl-secs", "3",
                *(extra or []),
            ]  # fmt: skip
        self.proc = subprocess.Popen(
            self.args, stdout=open(self.dir / "daemon.log", "wb"), stderr=subprocess.STDOUT
        )
        self.client = vmlayer.Client(self.endpoint)
        eventually(lambda: sock.exists() and self._healthy())

    def _healthy(self) -> bool:
        try:
            return self.client.health()
        except vmlayer.VmlayerError:
            return False

    def restart(self) -> None:
        self.proc = subprocess.Popen(
            self.args, stdout=open(self.dir / "daemon.log", "ab"), stderr=subprocess.STDOUT
        )
        self.client = vmlayer.Client(self.endpoint)
        eventually(self._healthy)

    def stop(self) -> None:
        for sb in self.client.list_sandboxes():
            sb.kill()
        self.proc.terminate()
        self.proc.wait(timeout=15)
        self.client.close()


@pytest.fixture(scope="module")
def hosts(daemon: vmlayer.Client) -> Iterator[tuple[Host, Host, Store]]:
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="cluster-", dir=root))
    store = Store(tmp)
    a, b = Host(tmp, "host-a", store), Host(tmp, "host-b", store)
    try:
        yield a, b, store
    finally:
        for h in (a, b):
            if h.proc.poll() is None:
                h.stop()
        if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
            shutil.rmtree(tmp, ignore_errors=True)


def test_registry_placement_and_lookup(hosts: tuple[Host, Host, Store]) -> None:
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


def test_admission_and_placement(daemon: vmlayer.Client) -> None:
    """A full host refuses creates; placement in the daemons routes around it."""
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="admit-", dir=root))
    store = Store(tmp)
    # host-small admits a single vCPU in total.
    ratio = 1.0 / (os.cpu_count() or 1) + 1e-6
    (tmp / "small.toml").write_text(f"[admission]\nvcpu_overcommit = {ratio}\n")
    small = Host(tmp, "host-small", store, ["--config", str(tmp / "small.toml")])
    big = Host(tmp, "host-big", store)
    try:
        cluster = Cluster(small.client)
        eventually(lambda: len(cluster.hosts()) == 2)
        first = Sandbox.create(ALPINE, client=small.client)
        with pytest.raises(vmlayer.ResourceExhausted, match="host is full"):
            Sandbox.create(ALPINE, client=small.client)
        eventually(
            lambda: any(
                h.host_id == "host-small" and h.allocated_vcpus == 1 for h in cluster.hosts()
            )
        )
        # The daemon places the next sandbox on the host with room.
        assert cluster.place().host_id == "host-big"
        placed = cluster.create(ALPINE)
        assert placed.id in {s.id for s in big.client.list_sandboxes()}
        with pytest.raises(vmlayer.ResourceExhausted):
            cluster.place(exclude=["host-big"])
        placed.kill()
        first.kill()
    finally:
        small.stop()
        big.stop()
        shutil.rmtree(tmp, ignore_errors=True)


def test_lease_allows_one_sandbox_cluster_wide(hosts: tuple[Host, Host, Store]) -> None:
    a, b, store = hosts
    first = Sandbox.create(ALPINE, lease="job-1", client=a.client)
    with pytest.raises(vmlayer.VmlayerError, match="held by host-a/"):
        Sandbox.create(ALPINE, lease="job-1", client=b.client)
    first.kill()  # releases the lease
    second = Sandbox.create(ALPINE, lease="job-1", client=b.client)

    # Host B dies without releasing anything. Its host lease (TTL 3 s) stops
    # being renewed, after which host A may take the name over, at a higher
    # epoch so anything B still tried to write would be rejected.
    owner = store.read_json("owners/job-1.json")
    if owner is not None:
        assert (owner["host"], owner["sandbox_id"]) == ("host-b", second.id)
    b.proc.kill()
    b.proc.wait()
    eventually(lambda: _claim(a), within=15)
    after = store.read_json("owners/job-1.json")
    if owner is not None:
        assert after["host"] == "host-a" and after["epoch"] > owner["epoch"]
    # A Docker sandbox has no shim to fence it: remove host B's leftover.
    subprocess.run(["docker", "rm", "-f", f"vmlayer-{second.id}"], capture_output=True, check=False)
    b.restart()


def _claim(host: Host) -> bool:
    try:
        Sandbox.create(ALPINE, lease="job-1", client=host.client).kill()
        return True
    except vmlayer.VmlayerError:
        return False


def test_stopped_host_leaves_the_registry(hosts: tuple[Host, Host, Store]) -> None:
    a, b, _ = hosts
    b.stop()
    eventually(lambda: [h.host_id for h in Cluster(a.client).hosts()] == ["host-a"])
