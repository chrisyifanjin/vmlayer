"""The Prometheus endpoint and per-request IDs (decision 0007)."""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import time
import urllib.request
from collections.abc import Iterator
from pathlib import Path

import pytest
from conftest import _daemon_bin
from test_auth import _free_port

import vmlayer
from vmlayer import Sandbox
from vmlayer.v1 import common_pb2


@pytest.fixture(scope="module")
def observed(daemon: vmlayer.Client) -> Iterator[tuple[vmlayer.Client, int]]:
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="metrics-", dir=root))
    port = _free_port()
    sock = tmp / "d.sock"
    (tmp / "vmlayerd.toml").write_text(
        f'listen = "unix://{sock}"\ndata_dir = "{tmp / "data"}"\n'
        f'[metrics]\nlisten = "127.0.0.1:{port}"\n'
    )
    proc = subprocess.Popen(
        [str(_daemon_bin()), "--config", str(tmp / "vmlayerd.toml")],
        stdout=open(tmp / "daemon.log", "wb"),
        stderr=subprocess.STDOUT,
    )
    client = vmlayer.Client(f"unix://{sock}")
    deadline = time.time() + 30
    while True:
        assert proc.poll() is None, f"daemon exited; see {tmp}/daemon.log"
        try:
            if client.health():
                break
        except vmlayer.VmlayerError:
            assert time.time() < deadline
            time.sleep(0.2)
    try:
        yield client, port
    finally:
        proc.terminate()
        proc.wait(timeout=15)
        shutil.rmtree(tmp, ignore_errors=True)


def scrape(port: int) -> str:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=10) as r:
        assert r.headers["Content-Type"].startswith("text/plain")
        return str(r.read().decode())


def test_operations_show_up_in_metrics(observed: tuple[vmlayer.Client, int]) -> None:
    client, port = observed
    with Sandbox.create("alpine:3.20", client=client) as sb:
        sb.exec("true")
        text = scrape(port)
        backend = sb.backend.value
        assert f'vmlayer_sandboxes{{backend="{backend}"}} 1' in text
        assert (
            f'vmlayer_sandbox_operations_total{{op="create",backend="{backend}",result="ok"}} 1'
            in text
        )
        assert 'vmlayer_rpc_requests_total{method="Processes/Exec",code="0"}' in text
        assert "# TYPE vmlayer_sandbox_operation_duration_seconds histogram" in text
        assert 'vmlayer_build_info{version="' in text
    with pytest.raises(vmlayer.SandboxNotFound):
        Sandbox.connect("sb-nope", client=client)
    assert 'vmlayer_rpc_requests_total{method="Sandboxes/Get",code="5"} 1' in scrape(port)


def test_every_rpc_returns_a_request_id(observed: tuple[vmlayer.Client, int]) -> None:
    client, _ = observed
    conn = client._conn
    _, call = conn.system.Health.with_call(common_pb2.Empty())
    ids = dict(call.initial_metadata())
    assert len(ids.get("x-request-id", "")) == 32
    _, call = conn.system.Health.with_call(
        common_pb2.Empty(), metadata=(("x-request-id", "caller-chosen-id"),)
    )
    assert dict(call.initial_metadata())["x-request-id"] == "caller-chosen-id"
