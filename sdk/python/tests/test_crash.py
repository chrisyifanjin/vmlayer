"""What happens to running microVMs when the daemon dies (review A1, T0.4, T0.5).

Needs a Firecracker daemon with a blob store and two shell commands to kill
it hard and to start it again:

    VMLAYER_FIRECRACKER_ENDPOINT=unix:///run/vmlayer/vmlayerd.sock
    VMLAYER_CRASH_KILL="sudo pkill -9 -x vmlayerd"
    VMLAYER_CRASH_START="VMLAYER_LEASE_TTL_SECS=6 scripts/kvm-vm/run-daemon.sh"

The daemon must run with a short lease TTL (VMLAYER_CRASH_LEASE_TTL, default
6 seconds) for the fencing test.
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
import uuid
from datetime import timedelta

import pytest

import vmlayer
from vmlayer import Sandbox, Volume

PY = "python:3.12-alpine"
ENDPOINT = os.environ.get("VMLAYER_FIRECRACKER_ENDPOINT")
KILL = os.environ.get("VMLAYER_CRASH_KILL")
START = os.environ.get("VMLAYER_CRASH_START")
TTL = float(os.environ.get("VMLAYER_CRASH_LEASE_TTL", "6"))

pytestmark = pytest.mark.skipif(
    not (ENDPOINT and KILL and START), reason="needs VMLAYER_CRASH_KILL/START and a KVM daemon"
)


def client() -> vmlayer.Client:
    return vmlayer.Client(ENDPOINT, timeout=timedelta(seconds=120))


def kill_daemon() -> None:
    subprocess.run(KILL, shell=True, check=False, capture_output=True)  # type: ignore[arg-type]
    time.sleep(0.5)


def start_daemon() -> vmlayer.Client:
    subprocess.run(START, shell=True, check=True, capture_output=True)  # type: ignore[arg-type]
    deadline = time.time() + 60
    while True:
        try:
            c = client()
            c.health()
            return c
        except vmlayer.VmlayerError:
            assert time.time() < deadline, "daemon did not come back"
            time.sleep(0.5)


def http_get(host: str, port: int, path: str) -> bytes:
    # A daemon in a local VM (scripts/kvm-vm) forwards new ports with a delay.
    deadline = time.time() + 15
    while True:
        try:
            conn = socket.create_connection((host, port), timeout=5)
            break
        except ConnectionRefusedError:
            if time.time() > deadline:
                raise
            time.sleep(0.25)
    with conn:
        conn.sendall(f"GET {path} HTTP/1.0\r\n\r\n".encode())
        data = b""
        while chunk := conn.recv(65536):
            data += chunk
    return data


def test_vm_services_survive_a_daemon_crash() -> None:
    c = client()
    if not c.info().blob_store:
        pytest.skip("needs a blob store")
    vol = Volume.create(size_mb=256, backend=vmlayer.Backend.FIRECRACKER, client=c)
    with Sandbox.create(PY, workspace=vol, entrypoint=None, client=c) as sb:
        sb.exec("head -c 40000000 /dev/urandom > /work/big && echo ok > /work/index.html && sync")
        checksum = sb.exec("sha256sum /work/big").stdout.split()[0]
    snap = vol.snapshot(durable=True)
    snap.delete(local_only=True)
    lazy = Volume.from_snapshot(snap.id, client=c)  # restored on demand
    serve = "exec python3 -m http.server -d /work 8080"
    sb = Sandbox.create(
        PY,
        workspace=lazy,
        entrypoint=["sh", "-c", serve],
        egress=["pypi.org", "*.pythonhosted.org"],
        client=c,
    )
    try:
        sb.wait_for_port(8080, timeout=timedelta(seconds=30))
        ep = sb.expose(8080)
        # Work the guest does while the daemon is dead: read the lazily
        # restored file (blocks fetched by the shim) and resolve a name
        # (the shim's DNS proxy).
        sb.spawn(
            "sleep 4; sha256sum /work/big > /tmp/sum; "
            "python3 -c \"import socket; print(socket.gethostbyname('pypi.org'))\" > /tmp/dns 2>&1"
        )
        kill_daemon()
        # The exposed port is served by the shim, so it still answers.
        time.sleep(2)
        assert b"ok" in http_get(ep.host, ep.port, "/index.html")
        time.sleep(8)
        c = start_daemon()
        back = Sandbox.connect(sb.id, client=c)
        assert back.exec("cat /tmp/sum").stdout.split()[0] == checksum
        assert back.exec("cat /tmp/dns").stdout.count(".") == 3
        sb = back
    finally:
        with pytest.MonkeyPatch.context():
            try:
                sb.kill()
            except vmlayer.VmlayerError:
                pass
        for v in (lazy, vol):
            try:
                v.delete()
            except vmlayer.VmlayerError:
                pass
        try:
            snap.delete()
        except vmlayer.VmlayerError:
            pass


def test_fencing_kills_the_vm_without_the_daemon() -> None:
    c = client()
    if not c.info().blob_store:
        pytest.skip("needs a blob store")
    name = f"crash-{uuid.uuid4().hex[:8]}"
    sb = Sandbox.create(PY, entrypoint=None, workspace=None, lease=name, client=c)
    assert sb.exec("echo alive").stdout == "alive\n"
    kill_daemon()
    # No daemon renews the host lease: the shim kills the VM at its deadline.
    time.sleep(TTL + 4)
    c = start_daemon()
    with pytest.raises(vmlayer.VmlayerError):
        Sandbox.connect(sb.id, client=c).exec("echo still here", timeout=timedelta(seconds=5))
    # And the name is free again for a new owner.
    Sandbox.create(PY, entrypoint=None, workspace=None, lease=name, client=c).kill()
