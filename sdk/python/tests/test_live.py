"""End-to-end tests through the real daemon and the Docker backend."""

from __future__ import annotations

import socket
import time
from datetime import timedelta
from pathlib import Path

import pytest

import vmlayer
from vmlayer import Sandbox, Volume

ALPINE = "alpine:3.20"
PY = "python:3.12-alpine"
SERVE = (
    "mkdir -p /srv && echo served > /srv/index.html && cd /srv && exec python3 -m http.server 8080"
)


def test_info(daemon: vmlayer.Client) -> None:
    info = daemon.info()
    assert info.daemon_version
    assert vmlayer.Backend.DOCKER in daemon.available_backends()


def test_pull_image(daemon: vmlayer.Client) -> None:
    info = daemon.pull_image(PY)
    assert (info.backend, info.rootfs_kind) == ("docker", "engine")
    assert info.entrypoint == ("python3",)
    with pytest.raises(vmlayer.VmlayerError):
        daemon.pull_image("vmlayer-test/does-not-exist:nope")


def test_exec_basics(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE, env={"GREETING": "hi"}, labels={"suite": "live"}) as sb:
        assert sb.backend == vmlayer.Backend.DOCKER
        assert sb.state == vmlayer.SandboxState.RUNNING
        assert sb.capabilities.snapshot and not sb.capabilities.fork

        r = sb.exec("echo $GREETING; pwd; echo oops >&2; exit 3")
        assert (r.exit_code, r.stdout, r.stderr) == (3, "hi\n/work\n", "oops\n")
        assert "oops" in r.output and "hi" in r.output
        assert not r.ok

        r = sb.exec(["sh", "-c", "pwd"], cwd="/tmp")
        assert r.stdout == "/tmp\n"
        r = sb.exec("cat", stdin=b"ping")
        assert r.stdout == "ping"
        r = sb.exec("echo $X", env={"X": "1"})
        assert r.stdout == "1\n"

        listed = daemon.list_sandboxes(labels={"suite": "live"})
        assert sb in listed
        assert sb.id == Sandbox.connect(sb.id).id

    with pytest.raises(vmlayer.SandboxNotFound):
        sb.refresh()
    sb.kill()  # idempotent


def test_exec_timeout_keeps_partial_output(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE) as sb:
        with pytest.raises(vmlayer.ExecTimeout) as ei:
            sb.exec("echo started; sleep 30; echo never", timeout=timedelta(seconds=2))
        assert ei.value.partial.stdout == "started\n"
        assert ei.value.partial.signal == 9


def test_spawn_stream_stdin_and_reattach(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE) as sb:
        p = sb.spawn("cat")
        p.stdin.write(b"a\n")
        p.stdin.write(b"b\n")
        p.stdin.close()
        chunks = [c.data for c in p]
        assert b"".join(chunks) == b"a\nb\n"
        assert p.wait().ok

        # Output is re-obtainable through a fresh handle after the fact.
        again = sb.process(p.pid)
        assert again.wait().stdout == "a\nb\n"

        # Kill a long runner.
        p = sb.spawn("sleep 60")
        time.sleep(0.5)
        assert any(pi.pid == p.pid for pi in sb.processes())
        p.kill()
        r = p.wait(timeout=timedelta(seconds=10))
        assert r.signal == 9


def test_files(daemon: vmlayer.Client, tmp_path: Path) -> None:
    with Sandbox.create(ALPINE) as sb:
        sb.files.write("/work/a/b.txt", "hello", mode=0o600)
        assert sb.files.read_text("/work/a/b.txt") == "hello"
        assert sb.files.exists("/work/a/b.txt") and not sb.files.exists("/work/nope")
        st = sb.files.stat("/work/a/b.txt")
        assert (st.size, st.mode, st.is_dir, st.name) == (5, 0o600, False, "b.txt")
        sb.files.mkdir("/work/a/d")
        names = [e.name for e in sb.files.list("/work/a")]
        assert names == ["b.txt", "d"]

        src = tmp_path / "tree"
        (src / "sub").mkdir(parents=True)
        (src / "x.txt").write_text("x")
        (src / "sub" / "y.txt").write_text("y")
        sb.files.upload(src, "/work/tree")
        assert sb.files.read_text("/work/tree/sub/y.txt") == "y"
        out = tmp_path / "out"
        sb.files.download("/work/tree", out)
        assert (out / "x.txt").read_text() == "x" and (out / "sub" / "y.txt").read_text() == "y"

        sb.files.remove("/work/a", recursive=True)
        assert not sb.files.exists("/work/a")
        with pytest.raises(vmlayer.VmlayerError):
            sb.files.read("/work/nope")


def test_workspace_volume_outlives_sandboxes(daemon: vmlayer.Client) -> None:
    vol = Volume.create(name="proj-live", size_mb=512, labels={"suite": "live"})
    try:
        with Sandbox.create(ALPINE, workspace=vol) as a:
            assert a.workspace == vol
            a.exec("echo v1 > /work/state.txt")
            assert vol.attached_to == a.id
            # single writer
            with pytest.raises(vmlayer.VolumeBusy):
                Sandbox.create(ALPINE, workspace="proj-live")
            # read-only mount of the same volume elsewhere is fine
            with Sandbox.create(ALPINE, mounts={"/data": (vol, "ro")}) as ro:
                assert ro.exec("cat /data/state.txt").stdout == "v1\n"
                assert not ro.exec("touch /data/x").ok
        assert vol.attached_to is None
        with Sandbox.create(PY, workspace=vol) as b:
            assert b.exec("cat /work/state.txt").stdout == "v1\n"
            b.exec("echo v2 > /work/state.txt")

        ckpt = vol.snapshot(labels={"suite": "live"})
        assert ckpt.volume_id == vol.id and ckpt.size_bytes > 0
        with Sandbox.create(ALPINE, workspace=vol) as c:
            c.exec("echo v3 > /work/state.txt")
        restored = Volume.from_snapshot(ckpt, name="proj-live-restored")
        try:
            with Sandbox.create(ALPINE, workspace=restored) as d:
                assert d.exec("cat /work/state.txt").stdout == "v2\n"
        finally:
            restored.delete()
        assert any(s.id == ckpt.id for s in daemon.list_volume_snapshots(labels={"suite": "live"}))
        ckpt.delete()
    finally:
        vol.delete()
    with pytest.raises(vmlayer.VolumeNotFound):
        Volume.get("proj-live")


def test_sandbox_snapshot_and_restore(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE, labels={"suite": "live"}) as sb:
        sb.exec("echo keep > /work/w.txt; echo root > /root.txt")
        snap = sb.snapshot(labels={"suite": "live"})
        assert not snap.includes_memory and snap.engine_version
        assert len(snap.volumes) == 1 and snap.volumes[0].guest_path == "/work"
        sb.exec("echo changed > /work/w.txt")
        assert sb.state == vmlayer.SandboxState.RUNNING

        with Sandbox.restore(snap) as r:
            assert r.exec("cat /work/w.txt").stdout == "keep\n"
            assert r.exec("cat /root.txt").stdout == "root\n"
            # the restored sandbox has its own clone; the original is untouched
            assert r.workspace is not None and sb.workspace is not None
            assert r.workspace.id != sb.workspace.id
            assert sb.exec("cat /work/w.txt").stdout == "changed\n"
        assert snap in daemon.list_snapshots(labels={"suite": "live"})
        snap.delete()
        with pytest.raises(vmlayer.SnapshotNotFound):
            Sandbox.restore(snap)
        with pytest.raises(vmlayer.BackendUnsupported):
            sb.fork(2)


def test_pause_resume(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE) as sb:
        sb.pause()
        assert sb.state == vmlayer.SandboxState.PAUSED
        sb.resume()
        assert sb.state == vmlayer.SandboxState.RUNNING


def test_entrypoint_wait_for_port_expose(daemon: vmlayer.Client) -> None:
    with Sandbox.create(
        PY,
        entrypoint=["sh", "-c", SERVE],
    ) as sb:
        sb.wait_for_port(8080, timeout=timedelta(seconds=20))
        assert any(p.is_entrypoint and p.pid == 1 for p in sb.processes())
        ep = sb.expose(8080)
        with socket.create_connection((ep.host, ep.port), timeout=5) as s:
            s.sendall(b"GET /index.html HTTP/1.0\r\nHost: x\r\n\r\n")
            data = b""
            while chunk := s.recv(4096):
                data += chunk
        assert b"served" in data
        assert sb.exposed == {8080: ep}
        sb.unexpose(8080)
        assert sb.exposed == {}
        with pytest.raises(vmlayer.TimeoutError):
            sb.wait_for_port(9999, timeout=timedelta(seconds=1))


def test_pty(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE) as sb, sb.pty("sh", size=vmlayer.PtySize(cols=100, rows=30)) as pty:
        pty.write(b"stty size; echo marker-$((1+1))\n")
        seen = b""
        deadline = time.time() + 15
        for chunk in pty:
            seen += chunk
            if b"marker-2" in seen or time.time() > deadline:
                break
        assert b"30 100" in seen and b"marker-2" in seen


def test_ttl_kills_sandbox(daemon: vmlayer.Client) -> None:
    sb = Sandbox.create(ALPINE, timeout=timedelta(seconds=3))
    deadline = time.time() + 20
    while time.time() < deadline:
        try:
            sb.refresh()
            time.sleep(0.5)
        except vmlayer.SandboxNotFound:
            break
    else:
        pytest.fail("sandbox outlived its ttl")


def test_no_workspace_and_image_entrypoint_default(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE, workspace=None, entrypoint=None) as sb:
        assert sb.workspace is None and sb.mounts == {}
        assert sb.exec("echo ok").stdout == "ok\n"
        assert sb.processes() == []


def test_egress_and_limits_need_an_enforcing_backend(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE) as sb:
        assert not sb.capabilities.egress_policy and not sb.capabilities.io_limits
    with pytest.raises(vmlayer.BackendUnsupported):
        Sandbox.create(ALPINE, egress=["pypi.org"])
    with pytest.raises(vmlayer.BackendUnsupported):
        Sandbox.create(ALPINE, limits=vmlayer.IoLimits(disk_mbps=10))
    with pytest.raises(vmlayer.InvalidArgument, match="egress rule"):
        Sandbox.create(ALPINE, egress=["not a rule"])
    with pytest.raises(TypeError):
        Sandbox.create(ALPINE, egress="pypi.org")  # type: ignore[arg-type]


def test_cloud_metadata_is_unreachable(daemon: vmlayer.Client) -> None:
    """Review T1.3: no sandbox reaches 169.254.169.254, and none can undo the block."""
    with Sandbox.create(ALPINE, timeout=timedelta(minutes=2)) as sb:
        if sb.backend != "docker":
            pytest.skip("the Firecracker suite covers microVMs")
        r = sb.exec("wget -T 3 -q -O- http://169.254.169.254/latest/meta-data/")
        assert r.exit_code != 0
        assert "blackhole 169.254.0.0/16" in sb.exec("ip route").stdout
        assert sb.exec("ip route del blackhole 169.254.0.0/16").exit_code != 0
        # Everything else still routes.
        assert sb.exec("ip route get 1.1.1.1").exit_code == 0


def test_exec_as_another_user(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE, timeout=timedelta(minutes=2)) as sb:
        assert sb.exec("id -u").stdout.strip() == "0"
        assert sb.exec("id -un", user="nobody").stdout.strip() == "nobody"
        assert sb.exec("id -u", user="1234:1234").stdout.strip() == "1234"
        r = sb.exec("touch /root/x", user="nobody")
        assert r.exit_code != 0, "an unprivileged user cannot write root's home"


def test_exists_under_a_missing_directory(daemon: vmlayer.Client) -> None:
    with Sandbox.create(ALPINE, timeout=timedelta(minutes=2)) as sb:
        assert not sb.files.exists("/no/such/dir/file")
        with pytest.raises(vmlayer.NotFound):
            sb.files.stat("/no/such/dir/file")
