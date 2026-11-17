"""End-to-end tests of the Firecracker backend on a real KVM host.

These need a running ``vmlayerd`` with the Firecracker backend, so they are
skipped unless ``VMLAYER_FIRECRACKER_ENDPOINT`` points at one:

    scripts/kvm-vm/up.sh
    export VMLAYER_FIRECRACKER_ENDPOINT=unix://$HOME/.vmlayer/kvm.sock
    uv run pytest tests/test_firecracker.py

CI runs them on a GitHub-hosted runner (x86_64, KVM).
"""

from __future__ import annotations

import os
import socket
import time
import uuid
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from datetime import timedelta

import pytest

import vmlayer
from vmlayer import Sandbox, Volume

PY = "python:3.12-alpine"
COUNTER = (
    "import os, time, itertools\n"
    "for i in itertools.count():\n"
    "    open('/work/n.tmp', 'w').write(str(i))\n"
    "    os.replace('/work/n.tmp', '/work/n')\n"
    "    time.sleep(0.02)\n"
)


@pytest.fixture(scope="module")
def fc() -> Iterator[vmlayer.Client]:
    endpoint = os.environ.get("VMLAYER_FIRECRACKER_ENDPOINT")
    if not endpoint:
        pytest.skip("VMLAYER_FIRECRACKER_ENDPOINT is not set")
    client = vmlayer.Client(endpoint, timeout=timedelta(seconds=120))
    info = client.info()
    backend = next((b for b in info.backends if b.name == "firecracker"), None)
    assert backend is not None, "the daemon has no firecracker backend"
    assert backend.available, backend.unavailable_reason
    client.pull_image(PY, backend=vmlayer.Backend.FIRECRACKER)
    yield client
    for sb in client.list_sandboxes():
        try:
            sb.kill()
        except vmlayer.VmlayerError:
            pass
    client.close()


def create(client: vmlayer.Client, **kwargs: object) -> Sandbox:
    kwargs.setdefault("entrypoint", None)
    kwargs.setdefault("memory_mb", 256)
    return Sandbox.create(PY, backend=vmlayer.Backend.FIRECRACKER, client=client, **kwargs)  # type: ignore[arg-type]


def counter(sb: Sandbox) -> int:
    return int(sb.files.read_text("/work/n"))


def advancing(sb: Sandbox) -> bool:
    before = counter(sb)
    time.sleep(0.3)
    return counter(sb) > before


def connect(host: str, port: int) -> socket.socket:
    # When the daemon runs in a VM on this machine (scripts/kvm-vm), the VM
    # forwards a newly opened port to the host after a short delay.
    deadline = time.time() + 15
    while True:
        try:
            return socket.create_connection((host, port), timeout=5)
        except ConnectionRefusedError:
            if time.time() > deadline:
                raise
            time.sleep(0.25)


def test_capabilities(fc: vmlayer.Client) -> None:
    backend = next(b for b in fc.info().backends if b.name == "firecracker")
    caps = backend.capabilities
    assert caps.memory_snapshot and caps.fork and caps.pty and caps.volumes


def test_boot_exec_files_and_identity(fc: vmlayer.Client) -> None:
    with create(fc, env={"GREETING": "hello"}) as sb:
        assert sb.backend == vmlayer.Backend.FIRECRACKER
        assert sb.exec("echo $GREETING").stdout == "hello\n"
        assert sb.exec("pwd").stdout == "/work\n"
        # A microVM runs its own kernel with the guest agent as PID 1.
        assert "vmlayer-guest-agent" in sb.exec("cat /proc/1/cmdline").stdout
        result = sb.exec("echo out; echo err >&2; exit 3")
        assert (result.exit_code, result.stdout, result.stderr) == (3, "out\n", "err\n")
        payload = os.urandom(1 << 20)
        sb.files.write("/work/blob", payload)
        assert sb.files.read("/work/blob") == payload
        assert sb.exec("python3 -c 'print(6 * 7)'").stdout == "42\n"


def test_exec_timeout(fc: vmlayer.Client) -> None:
    with create(fc) as sb:
        with pytest.raises(vmlayer.ExecTimeout) as caught:
            sb.exec("echo started; sleep 30", timeout=timedelta(seconds=1))
        assert "started" in caught.value.partial.stdout
        assert sb.exec("echo still-usable").ok


def test_egress(fc: vmlayer.Client) -> None:
    probe = "import socket; socket.create_connection(('1.1.1.1', 443), 10).close(); print('ok')"
    with create(fc) as sb:
        result = sb.exec(["python3", "-c", probe], timeout=timedelta(seconds=30))
        assert result.stdout == "ok\n", result.output


def reachable(sb: Sandbox, host: str, port: int) -> str:
    """'ok', or why a TCP connection from the guest failed."""
    probe = (
        "import socket, sys\n"
        "try:\n"
        f"    socket.create_connection(({host!r}, {port}), 5).close(); print('ok')\n"
        "except Exception as e:\n"
        "    print(type(e).__name__)\n"
    )
    return sb.exec(["python3", "-c", probe], timeout=timedelta(seconds=30)).stdout.strip()


def test_metadata_endpoint_is_always_blocked(fc: vmlayer.Client) -> None:
    with create(fc) as sb:
        assert reachable(sb, "1.1.1.1", 443) == "ok"
        assert reachable(sb, "169.254.169.254", 80) != "ok"


def test_egress_allowlist(fc: vmlayer.Client) -> None:
    rules = ["1.1.1.1:443", "pypi.org", "*.pythonhosted.org:443"]
    with create(fc, egress=rules) as sb:
        assert reachable(sb, "1.1.1.1", 443) == "ok"
        # Not allowed: a different address, and an allowed address on another port.
        assert reachable(sb, "8.8.8.8", 443) == "ConnectionRefusedError"
        assert reachable(sb, "1.1.1.1", 80) == "ConnectionRefusedError"
        # Names resolve only if allowed, and their addresses open with them.
        assert reachable(sb, "pypi.org", 443) == "ok"
        assert reachable(sb, "files.pythonhosted.org", 443) == "ok"
        assert reachable(sb, "example.com", 443) == "gaierror"
        # Pointing at another resolver does not get around the policy.
        out = sb.exec("nslookup example.com 8.8.8.8 || true").stdout
        assert "NXDOMAIN" in out or "can't find" in out, out
    with create(fc, egress=[]) as sb:
        assert reachable(sb, "1.1.1.1", 443) == "ConnectionRefusedError"


def test_sandboxes_cannot_reach_the_host_or_each_other(fc: vmlayer.Client) -> None:
    serve = "mkdir -p /srv && cd /srv && echo hi > i && exec python3 -m http.server 8080"
    with create(fc, entrypoint=["sh", "-c", serve]) as a, create(fc) as b:
        a.wait_for_port(8080, timeout=timedelta(seconds=30))
        # The daemon reaches a through a's link address...
        endpoint = a.expose(8080)
        with connect(endpoint.host, endpoint.port):
            pass
        # ...but b reaches neither the host end of any VM link (sshd listens
        # there on the CI runner and the dev VM) nor any VM's link address.
        hosts = [f"10.200.0.{4 * i + 1}" for i in range(16)]
        peers = [f"10.200.0.{4 * i + 2}" for i in range(16)]
        probe = (
            "import socket, sys, concurrent.futures as cf\n"
            "def try_(t):\n"
            "    try:\n"
            "        socket.create_connection(t, 1).close(); return t\n"
            "    except Exception:\n"
            "        return None\n"
            f"targets = [(h, 22) for h in {hosts!r}] + [(p, 8080) for p in {peers!r}]\n"
            "with cf.ThreadPoolExecutor(32) as ex:\n"
            "    print([t for t in ex.map(try_, targets) if t])\n"
        )
        out = b.exec(["python3", "-c", probe], timeout=timedelta(seconds=30)).stdout.strip()
        assert out == "[]", f"b reached {out}"


def test_io_limits(fc: vmlayer.Client) -> None:
    caps = next(b for b in fc.info().backends if b.name == "firecracker").capabilities
    assert caps.egress_policy and caps.io_limits

    def write_seconds(limits: vmlayer.IoLimits | None) -> float:
        cmd = "dd if=/dev/zero of=/work/f bs=1M count=24 oflag=direct"
        with create(fc, limits=limits) as sb:
            start = time.perf_counter()
            assert sb.exec(cmd, timeout=timedelta(seconds=60)).ok
            return time.perf_counter() - start

    # 24 MiB at 4 MB/s: about 5 s once the first second's budget is spent.
    limited, free = write_seconds(vmlayer.IoLimits(disk_mbps=4)), write_seconds(None)
    assert limited > 3, limited
    assert free < 1.5, free

    def download_seconds(limits: vmlayer.IoLimits | None) -> float:
        serve = "head -c 8000000 /dev/zero > /tmp/blob && exec python3 -m http.server -d /tmp 8080"
        with create(fc, entrypoint=["sh", "-c", serve], limits=limits) as sb:
            sb.wait_for_port(8080, timeout=timedelta(seconds=30))
            endpoint = sb.expose(8080)
            with connect(endpoint.host, endpoint.port) as conn:
                start = time.perf_counter()
                conn.sendall(b"GET /blob HTTP/1.0\r\n\r\n")
                got = 0
                while chunk := conn.recv(1 << 16):
                    got += len(chunk)
                elapsed = time.perf_counter() - start
            assert got > 8_000_000
            return elapsed

    # 8 MB at 16 Mbit/s (2 MB/s): about 3 s.
    limited, free = download_seconds(vmlayer.IoLimits(network_mbps=16)), download_seconds(None)
    assert limited > 2, limited
    assert free < limited / 2, (free, limited)


def test_volume_outlives_sandboxes(fc: vmlayer.Client) -> None:
    volume = Volume.create(name=f"fc-{uuid.uuid4().hex[:8]}", size_mb=256, client=fc)
    try:
        # Killed right after the write: the guest must flush the volume first.
        with create(fc, workspace=volume) as first:
            first.exec("echo kept > /work/state")
        with create(fc, workspace=volume) as second:
            assert second.exec("cat /work/state").stdout == "kept\n"
            second.exec("echo paused > /work/state")
            second.pause()  # a paused guest still has to flush before it dies
        with create(fc, workspace=volume) as third:
            assert third.exec("cat /work/state").stdout == "paused\n"
    finally:
        volume.delete()


def test_durable_block_volume_snapshot(fc: vmlayer.Client) -> None:
    if not fc.info().blob_store:
        pytest.skip("the daemon has no blob store")
    volume = Volume.create(size_mb=256, backend=vmlayer.Backend.FIRECRACKER, client=fc)
    restored: Volume | None = None
    try:
        with create(fc, workspace=volume) as sb:
            sb.exec("head -c 5000000 /dev/urandom > /work/data && echo hi > /work/note")
            checksum = sb.exec("sha256sum /work/data").stdout.split()[0]
        snap = volume.snapshot(durable=True)
        assert snap.durable
        # Only the blob store has it now: restoring downloads the image.
        snap.delete(local_only=True)
        restored = Volume.from_snapshot(snap.id, client=fc)
        with create(fc, workspace=restored) as sb:
            assert sb.exec("cat /work/note").stdout == "hi\n"
            assert sb.exec("sha256sum /work/data").stdout.split()[0] == checksum
        snap.delete()
    finally:
        if restored is not None:
            restored.delete()
        volume.delete()


def test_lazy_restore_of_a_large_volume(fc: vmlayer.Client) -> None:
    if not fc.info().blob_store:
        pytest.skip("the daemon has no blob store")
    volume = Volume.create(size_mb=1024, backend=vmlayer.Backend.FIRECRACKER, client=fc)
    restored: Volume | None = None
    try:
        with create(fc, workspace=volume) as sb:
            sb.exec(
                "head -c 300000000 /dev/urandom > /work/big && echo marker > /work/note && sync",
                timeout=timedelta(minutes=2),
            )
            checksum = sb.exec("sha256sum /work/big", timeout=timedelta(minutes=2)).stdout.split()[
                0
            ]
        snap = volume.snapshot(durable=True)
        snap.delete(local_only=True)
        start = time.perf_counter()
        restored = Volume.from_snapshot(snap.id, client=fc)
        returned = time.perf_counter() - start
        with create(fc, workspace=restored) as sb:
            first = sb.exec("cat /work/note").stdout
            ready = time.perf_counter() - start
            assert first == "marker\n"
            full = sb.exec("sha256sum /work/big", timeout=timedelta(minutes=2)).stdout.split()[0]
            assert full == checksum
        print(f"from_snapshot {returned:.2f}s, first file read {ready:.2f}s")
        snap.delete()
    finally:
        if restored is not None:
            restored.delete()
        volume.delete()


def test_durable_memory_snapshot(fc: vmlayer.Client) -> None:
    if not fc.info().blob_store:
        pytest.skip("the daemon has no blob store")
    with create(fc) as sb:
        sb.files.write("/work/counter.py", COUNTER)
        sb.spawn("python3 /work/counter.py")
        time.sleep(1)
        snap = sb.snapshot(durable=True)
    assert snap.durable and snap.includes_memory
    assert snap.id in {s.id for s in fc.list_snapshots(remote=True)}
    # Only the blob store has it now; restore downloads it and resumes.
    snap.delete(local_only=True)
    assert snap.id not in {s.id for s in fc.list_snapshots()}
    restored = Sandbox.restore(snap.id, client=fc)
    try:
        assert advancing(restored), "the process running at snapshot time did not resume"
    finally:
        restored.kill()
    snap.delete()
    assert snap.id not in {s.id for s in fc.list_snapshots(remote=True)}


def test_memory_snapshot_restore_and_fork(fc: vmlayer.Client) -> None:
    with create(fc) as sb:
        sb.files.write("/work/counter.py", COUNTER)
        writer = sb.spawn("python3 /work/counter.py")
        time.sleep(1)
        assert advancing(sb)

        snapshot = sb.snapshot()
        assert snapshot.includes_memory
        assert advancing(sb), "the parent stalled after its snapshot"

        restored = Sandbox.restore(snapshot, client=fc)
        try:
            # The process was running when the snapshot was taken, so it is
            # running in the restored VM without being started again.
            assert advancing(restored)
            assert restored.exec("echo fresh").stdout == "fresh\n"
            assert restored.workspace is not None and sb.workspace is not None
            assert restored.workspace.id != sb.workspace.id
        finally:
            restored.kill()
            snapshot.delete()

        children = sb.fork(2)
        try:
            for child in children:
                assert advancing(child)
                child.exec(f"echo {child.id} > /work/who")
            for child in children:
                assert child.exec("cat /work/who").stdout.strip() == child.id
            assert not sb.files.exists("/work/who")
        finally:
            for child in children:
                child.kill()

        # The handle from before three guest resets still controls the process.
        assert advancing(sb)
        writer.kill()
        assert writer.wait(timeout=timedelta(seconds=10)).signal == 9


def test_pty(fc: vmlayer.Client) -> None:
    with create(fc) as sb, sb.pty("sh", size=vmlayer.PtySize(cols=100, rows=30)) as pty:
        pty.write(b"stty size; echo marker-$((1+1))\n")
        seen = b""
        deadline = time.time() + 15
        for chunk in pty:
            seen += chunk
            if b"marker-2" in seen or time.time() > deadline:
                break
        assert b"30 100" in seen and b"marker-2" in seen


def test_entrypoint_wait_for_port_expose(fc: vmlayer.Client) -> None:
    serve = (
        "mkdir -p /srv && echo served > /srv/index.html && cd /srv"
        " && exec python3 -m http.server 8080"
    )
    with create(fc, entrypoint=["sh", "-c", serve]) as sb:
        sb.wait_for_port(8080, timeout=timedelta(seconds=30))
        endpoint = sb.expose(8080)
        with connect(endpoint.host, endpoint.port) as conn:
            conn.sendall(b"GET /index.html HTTP/1.0\r\nHost: x\r\n\r\n")
            data = b""
            while chunk := conn.recv(4096):
                data += chunk
        assert b"served" in data


def test_sandboxes_without_a_volume_keep_their_own_identity(fc: vmlayer.Client) -> None:
    # With boot templates enabled, the creates after the first are restored
    # from one shared snapshot; each must still get its own environment.
    for i in range(3):
        with create(fc, workspace=None, env={"WHO": f"sandbox-{i}"}) as sb:
            assert sb.exec("echo $WHO").stdout == f"sandbox-{i}\n"
            assert sb.exec("python3 -c 'print(1)'").ok
        if i == 0:
            time.sleep(8)  # let the background template build finish


def test_burst_of_creates_beyond_the_cpu_count(fc: vmlayer.Client) -> None:
    # Twice the vCPUs of the CI runner and the scripts/kvm-vm VM (4 each).
    count = int(os.environ.get("VMLAYER_BURST", "8"))

    def boot(i: int) -> Sandbox:
        sb = create(fc, memory_mb=128)
        assert sb.exec(f"echo {i}").stdout == f"{i}\n"
        return sb

    with ThreadPoolExecutor(count) as pool:
        futures = [pool.submit(boot, i) for i in range(count)]
        sandboxes, errors = [], []
        for future in futures:
            try:
                sandboxes.append(future.result())
            except Exception as error:
                errors.append(f"{type(error).__name__}: {error}")
    for sb in sandboxes:
        sb.kill()
    assert not errors, errors
