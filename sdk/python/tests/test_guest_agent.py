"""Runs the static Linux guest agent as PID 1 inside a container and talks to
its gRPC services over TCP: everything a microVM would do except vsock and
mounts. Needs the binary from `scripts/build-guest-agent.sh` (or a musl
build) in VMLAYER_GUEST_AGENT_BIN, and Docker; skipped otherwise.
"""

from __future__ import annotations

import os
import subprocess
import time
from collections.abc import Iterator

import grpc
import pytest

from vmlayer.guest.v1 import guest_pb2 as g
from vmlayer.guest.v1 import guest_pb2_grpc as gg
from vmlayer.v1 import common_pb2 as c
from vmlayer.v1 import files_pb2 as f
from vmlayer.v1 import files_pb2_grpc as fg
from vmlayer.v1 import processes_pb2 as p
from vmlayer.v1 import processes_pb2_grpc as pg

PORT = int(os.environ.get("VMLAYER_GUEST_AGENT_TEST_PORT", "5299"))


@pytest.fixture(scope="module")
def agent() -> Iterator[tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub]]:
    binary = os.environ.get("VMLAYER_GUEST_AGENT_BIN")
    if not binary or not os.path.exists(binary):
        pytest.skip("VMLAYER_GUEST_AGENT_BIN not set; build with scripts/build-guest-agent.sh")
    name = f"vmlayer-agent-test-{os.getpid()}"
    subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    run = subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            name,
            "--init=false",
            "-p",
            f"127.0.0.1:{PORT}:5200",
            "-v",
            f"{os.path.abspath(binary)}:/vmlayer-guest-agent:ro",
            "python:3.12-alpine",
            "/vmlayer-guest-agent",
            "--listen",
            "tcp://0.0.0.0:5200",
        ],
        capture_output=True,
        text=True,
    )
    if run.returncode != 0:
        pytest.skip(f"docker run failed: {run.stderr.strip()}")
    ch = grpc.insecure_channel(f"127.0.0.1:{PORT}")
    guest = gg.GuestStub(ch)
    deadline = time.time() + 20
    while True:
        try:
            guest.Ready(c.Empty(), timeout=2)
            break
        except grpc.RpcError:
            if time.time() > deadline:
                logs = subprocess.run(
                    ["docker", "logs", name], capture_output=True, text=True
                ).stderr
                subprocess.run(["docker", "rm", "-f", name], capture_output=True)
                pytest.fail(f"agent never became ready:\n{logs}")
            time.sleep(0.2)
    yield guest, pg.ProcessesStub(ch), fg.FilesStub(ch)
    subprocess.run(["docker", "rm", "-f", name], capture_output=True)


def _events(stream: Iterator[p.ExecEvent]) -> tuple[bytes, c.ExitStatus]:
    out = b""
    exited = None
    for e in stream:
        kind = e.WhichOneof("event")
        if kind == "output":
            out += e.output.data
        elif kind == "exited":
            exited = e.exited
    assert exited is not None
    return out, exited


def test_ready_reports_versions(agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub]) -> None:
    guest, _, _ = agent
    r = guest.Ready(c.Empty(), timeout=5)
    assert r.agent_version and r.kernel


def test_exec_env_cwd_exit_code(agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub]) -> None:
    _, procs, _ = agent
    out, ex = _events(
        procs.Exec(
            p.ExecRequest(
                argv=["sh", "-c", "echo $X; pwd; echo err >&2; exit 4"], env={"X": "1"}, cwd="/tmp"
            ),
            timeout=10,
        )
    )
    assert ex.code == 4 and b"1\n" in out and b"/tmp\n" in out and b"err\n" in out


def test_stdin_round_trip_and_reattach(
    agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub],
) -> None:
    _, procs, _ = agent
    started = next(procs.Exec(p.ExecRequest(argv=["cat"], detach=True), timeout=10))
    pid = started.started.pid

    def gen() -> Iterator[p.StdinChunk]:
        yield p.StdinChunk(pid=pid, data=b"hello ")
        yield p.StdinChunk(data=b"stdin")

    procs.Stdin(gen(), timeout=10)
    out, ex = _events(procs.Output(p.OutputRequest(pid=pid), timeout=10))
    assert out == b"hello stdin" and ex.code == 0
    # history is replayed for a second reader
    out2, _ = _events(procs.Output(p.OutputRequest(pid=pid), timeout=10))
    assert out2 == b"hello stdin"


def test_timeout_kills_and_flags(
    agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub],
) -> None:
    _, procs, _ = agent
    _, ex = _events(
        procs.Exec(p.ExecRequest(argv=["sleep", "30"], timeout={"seconds": 1}), timeout=10)
    )
    assert ex.signal == 9 and ex.timed_out


def test_kill_and_list(agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub]) -> None:
    _, procs, _ = agent
    pid = next(procs.Exec(p.ExecRequest(argv=["sleep", "60"], detach=True), timeout=10)).started.pid
    assert pid in [x.pid for x in procs.List(p.ListProcessesRequest()).processes]
    procs.Kill(p.KillProcessRequest(pid=pid, signal=9))
    assert procs.Wait(p.WaitRequest(pid=pid), timeout=10).signal == 9
    assert pid not in [x.pid for x in procs.List(p.ListProcessesRequest()).processes]


def test_files(agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub]) -> None:
    _, _, files = agent

    def wgen() -> Iterator[f.WriteFileRequest]:
        yield f.WriteFileRequest(header=f.WriteFileRequest.Header(path="/work/a/b.txt", mode=0o600))
        yield f.WriteFileRequest(data=b"file")
        yield f.WriteFileRequest(data=b"data")

    files.Write(wgen(), timeout=5)
    assert (
        b"".join(ch.data for ch in files.Read(f.ReadFileRequest(path="/work/a/b.txt")))
        == b"filedata"
    )
    st = files.Stat(f.StatRequest(path="/work/a/b.txt"))
    assert (st.size, st.mode, st.is_dir) == (8, 0o600, False)
    files.Mkdir(f.MkdirRequest(path="/work/a/d"))
    assert [e.path for e in files.List(f.ListFilesRequest(path="/work/a")).entries] == [
        "/work/a/b.txt",
        "/work/a/d",
    ]
    with pytest.raises(grpc.RpcError) as ei:
        files.Stat(f.StatRequest(path="/nope"))
    assert ei.value.code() == grpc.StatusCode.NOT_FOUND
    files.Remove(f.RemoveRequest(path="/work/a", recursive=True))
    with pytest.raises(grpc.RpcError):
        files.Stat(f.StatRequest(path="/work/a"))


def test_entrypoint_wait_for_port_pid1_alias(
    agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub],
) -> None:
    guest, procs, _ = agent
    st = guest.StartEntrypoint(
        g.EntrypointRequest(argv=["python3", "-m", "http.server", "8080"], cwd="/tmp"), timeout=5
    )
    assert st.pid == 1
    guest.WaitForPort(g.WaitForPortRequest(port=8080, timeout={"seconds": 15}), timeout=20)
    listed = procs.List(p.ListProcessesRequest()).processes
    assert any(x.pid == 1 and x.is_entrypoint for x in listed)
    with pytest.raises(grpc.RpcError) as ei:
        guest.WaitForPort(g.WaitForPortRequest(port=9999, timeout={"seconds": 1}), timeout=5)
    assert ei.value.code() == grpc.StatusCode.DEADLINE_EXCEEDED
    procs.Kill(p.KillProcessRequest(pid=1, signal=9))
    assert procs.Wait(p.WaitRequest(pid=1), timeout=10).signal == 9


def test_pty(agent: tuple[gg.GuestStub, pg.ProcessesStub, fg.FilesStub]) -> None:
    _, procs, _ = agent

    def pgen() -> Iterator[p.PtyInput]:
        yield p.PtyInput(open=p.PtyOpen(argv=["sh"], size=p.PtySize(cols=100, rows=30)))
        time.sleep(0.8)
        yield p.PtyInput(data=b"stty size\n")
        time.sleep(0.3)
        yield p.PtyInput(data=b"echo marker-$((1+1))\n")
        time.sleep(0.3)
        yield p.PtyInput(data=b"exit\n")
        time.sleep(0.3)

    seen = b""
    exited = None
    for m in procs.Pty(pgen(), timeout=20):
        kind = m.WhichOneof("output")
        if kind == "data":
            seen += m.data
        elif kind == "exited":
            exited = m.exited
    assert b"30 100" in seen and b"marker-2" in seen
    assert exited is not None and exited.code == 0
