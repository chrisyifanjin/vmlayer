"""A host dies; another daemon brings its rescheduled sandbox back."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from test_cluster import Host, eventually

import vmlayer
from vmlayer import Sandbox

ALPINE = "alpine:3.20"


@pytest.fixture
def pair(daemon: vmlayer.Client) -> Iterator[tuple[Host, Host]]:
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="rescue-", dir=root))
    store = tmp / "blobs"
    extra = ["--checkpoint-secs", "1"]
    a = Host(tmp, "host-a", store, extra)
    b = Host(tmp, "host-b", store, [*extra, "--rescue"])
    try:
        yield a, b
    finally:
        for h in (a, b):
            if h.proc.poll() is None:
                h.stop()
        if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
            shutil.rmtree(tmp, ignore_errors=True)


def checkpoints(client: vmlayer.Client) -> list[vmlayer.VolumeSnapshot]:
    snaps = client.list_volume_snapshots(remote=True)
    return [s for s in snaps if s.labels.get("vmlayer.workload") == "svc"]


def test_rescheduled_sandbox_survives_its_host(pair: tuple[Host, Host]) -> None:
    a, b = pair
    sb = Sandbox.create(ALPINE, lease="svc", reschedule=True, client=a.client)
    sb.exec("echo kept > /work/data.txt")
    written = time.time()
    # A checkpoint taken after the write is in the blob store.
    eventually(lambda: any(s.created_at > written for s in checkpoints(a.client)))

    # Host A dies: its daemon and its sandbox are gone without any cleanup.
    a.proc.kill()
    a.proc.wait()
    subprocess.run(["docker", "rm", "-f", f"vmlayer-{sb.id}"], check=False, capture_output=True)

    def rescued() -> list[Sandbox]:
        return [s for s in b.client.list_sandboxes() if s.labels.get("vmlayer.lease") == "svc"]

    eventually(lambda: len(rescued()) == 1, within=30)
    back = rescued()[0]
    assert back.id != sb.id
    assert back.exec("cat /work/data.txt").stdout == "kept\n"

    # Killing it on purpose ends the workload: no checkpoint, no rescue.
    back.kill()
    eventually(lambda: not checkpoints(b.client))
    time.sleep(6)
    assert not rescued()


def test_reschedule_needs_a_lease(daemon: vmlayer.Client) -> None:
    with pytest.raises(vmlayer.VmlayerError, match="lease"):
        Sandbox.create(ALPINE, reschedule=True)
