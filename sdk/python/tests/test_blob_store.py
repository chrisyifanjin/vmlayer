"""Durable volume snapshots through a blob store shared by two daemons.

The session daemon (conftest) uses a directory as its blob store, standing in
for S3 or Azure Blob. A second daemon with its own data directory and the
same store plays another host.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from conftest import _daemon_bin

import vmlayer
from vmlayer import Sandbox, Volume

ALPINE = "alpine:3.20"


@pytest.fixture(scope="module")
def other_host(daemon: vmlayer.Client) -> Iterator[vmlayer.Client]:
    store = daemon.info().blob_store
    assert store and store.startswith("file://"), store
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = tempfile.mkdtemp(prefix="host-b-", dir=root)
    sock = Path(tmp) / "d.sock"
    log = open(Path(tmp) / "daemon.log", "wb")
    proc = subprocess.Popen(
        [
            str(_daemon_bin()),
            "--listen",
            f"unix://{sock}",
            "--data-dir",
            str(Path(tmp) / "data"),
            "--blob-store",
            store,
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    client = vmlayer.Client(f"unix://{sock}")
    deadline = time.time() + 30
    while not sock.exists() or not _healthy(client):
        if time.time() > deadline or proc.poll() is not None:
            proc.kill()
            raise RuntimeError(f"second daemon did not start; see {tmp}/daemon.log")
        time.sleep(0.1)
    yield client
    for sb in client.list_sandboxes():
        sb.kill()
    proc.terminate()
    proc.wait(timeout=10)
    client.close()
    if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
        shutil.rmtree(tmp, ignore_errors=True)


def _healthy(client: vmlayer.Client) -> bool:
    try:
        return client.health()
    except vmlayer.VmlayerError:
        return False


def test_durable_snapshot_restores_on_another_daemon(
    daemon: vmlayer.Client, other_host: vmlayer.Client
) -> None:
    vol = Volume.create(size_mb=64, labels={"test": "blob"})
    try:
        with Sandbox.create(ALPINE, workspace=vol) as sb:
            sb.exec("mkdir -p src && echo 'print(42)' > src/app.py && chmod 755 src/app.py")
            sb.exec("ln -s src/app.py link && dd if=/dev/urandom of=big bs=1M count=6")
            checksum = sb.exec("sha256sum big").stdout.split()[0]
            snap = vol.snapshot(durable=True, labels={"test": "blob"})
        assert snap.durable and snap.local

        listed = {s.id: s for s in other_host.list_volume_snapshots(remote=True)}
        assert snap.id in listed and not listed[snap.id].local
        assert listed[snap.id].labels["test"] == "blob"

        copy = Volume.from_snapshot(snap.id, client=other_host)
        assert copy.size_mb == 64
        with Sandbox.create(ALPINE, workspace=copy, client=other_host) as sb:
            assert sb.exec("cat link").stdout == "print(42)\n"
            assert sb.exec("stat -c %a src/app.py").stdout == "755\n"
            assert sb.exec("sha256sum big").stdout.split()[0] == checksum
        copy.delete()

        # Free the local copy; the snapshot is still restorable here.
        snap.delete(local_only=True)
        mine = {s.id: s for s in daemon.list_volume_snapshots()}
        assert not mine[snap.id].local
        again = Volume.from_snapshot(snap.id)
        with Sandbox.create(ALPINE, workspace=again) as sb:
            assert sb.exec("cat src/app.py").stdout == "print(42)\n"
        again.delete()

        assert daemon.collect_garbage(dry_run=True).snapshots >= 1
        snap.delete()
        assert snap.id not in {s.id for s in other_host.list_volume_snapshots(remote=True)}
        with pytest.raises(vmlayer.VmlayerError):
            Volume.from_snapshot(snap.id, client=other_host)
    finally:
        vol.delete()


def test_info_reports_the_blob_store(daemon: vmlayer.Client) -> None:
    assert (daemon.info().blob_store or "").startswith("file://")
