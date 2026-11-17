"""Spins up a private vmlayerd for the live test session.

Set VMLAYERD_BIN to a prebuilt daemon; otherwise the one in the workspace's
target/debug is used (build it with `cargo build -p vmlayerd`). Tests skip
when the daemon has no usable backend (no Docker on this machine).
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

import vmlayer

REPO = Path(__file__).resolve().parents[3]


def _daemon_bin() -> Path:
    if env := os.environ.get("VMLAYERD_BIN"):
        return Path(env)
    candidate = REPO / "target" / "debug" / "vmlayerd"
    if not candidate.exists():
        subprocess.run(["cargo", "build", "-p", "vmlayerd"], cwd=REPO, check=True)
    return candidate


@pytest.fixture(scope="session")
def daemon() -> Iterator[vmlayer.Client]:
    if os.environ.get("VMLAYER_LIVE_TESTS", "1") != "1":
        pytest.skip("VMLAYER_LIVE_TESTS=0")
    # Under $HOME, not /tmp: VM-based Docker engines (Docker Desktop, colima)
    # share the home directory with the engine, so bind mounts of volume
    # directories actually reach the host. The daemon verifies this at start.
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    root.mkdir(parents=True, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="run-", dir=root)
    sock = Path(tmp) / "d.sock"
    env = dict(os.environ, VMLAYER_LOG="info,vmlayerd=debug")
    log = open(Path(tmp) / "daemon.log", "wb")
    proc = subprocess.Popen(
        [
            str(_daemon_bin()),
            "--listen",
            f"unix://{sock}",
            "--data-dir",
            str(Path(tmp) / "data"),
            # A shared directory stands in for S3: test_blob_store.py starts a
            # second daemon on the same store to act as another host.
            "--blob-store",
            f"file://{Path(tmp) / 'blobs'}",
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
        env=env,
    )
    client = vmlayer.Client(f"unix://{sock}")
    deadline = time.time() + 30
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"daemon exited early; see {tmp}/daemon.log")
        try:
            if client.health():
                break
        except vmlayer.VmlayerError:
            time.sleep(0.1)
    else:
        proc.kill()
        raise RuntimeError(f"daemon did not become healthy; see {tmp}/daemon.log")
    info = client.info()
    if not any(b.available for b in info.backends):
        proc.terminate()
        pytest.skip(
            "no backend available on this host: "
            + "; ".join(b.unavailable_reason for b in info.backends)
        )
    vmlayer.set_default_client(client)
    yield client
    for sb in client.list_sandboxes():
        try:
            sb.kill()
        except vmlayer.VmlayerError:
            pass
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    client.close()
    print(f"daemon log: {tmp}/daemon.log")
    if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
        shutil.rmtree(tmp, ignore_errors=True)
