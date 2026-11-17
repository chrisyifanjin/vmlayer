"""The E2B-shaped facade against the session daemon (Docker backend)."""

from __future__ import annotations

import socket
import time

import pytest

import vmlayer
from vmlayer.e2b import (
    CommandExitException,
    NotFoundException,
    Sandbox,
    create_template,
)

IMAGE = "python:3.12-alpine"


def test_commands_files_and_lifecycle(daemon: vmlayer.Client) -> None:
    with Sandbox.create(IMAGE, timeout=120, metadata={"user": "u1"}, envs={"MODE": "e2b"}) as sbx:
        assert sbx.sandbox_id.startswith("sb-")
        r = sbx.commands.run("echo $MODE; echo oops >&2")
        assert (r.stdout, r.stderr, r.exit_code) == ("e2b\n", "oops\n", 0)
        assert sbx.commands.run("pwd", cwd="/tmp").stdout == "/tmp\n"
        with pytest.raises(CommandExitException) as caught:
            sbx.commands.run("echo partial; exit 3")
        assert caught.value.exit_code == 3 and caught.value.stdout == "partial\n"

        handle = sbx.commands.run("sleep 30", background=True)
        assert handle.pid in {p.pid for p in sbx.commands.list()}
        assert sbx.commands.kill(handle.pid)
        seen: list[str] = []
        sbx.commands.run("echo streamed", on_stdout=seen.append)
        assert "".join(seen) == "streamed\n"

        sbx.files.make_dir("/tmp/d")
        sbx.files.write("/tmp/d/a.txt", "hello")
        assert sbx.files.read("/tmp/d/a.txt") == "hello"
        assert sbx.files.read("/tmp/d/a.txt", format="bytes") == b"hello"
        assert [e.name for e in sbx.files.list("/tmp/d")] == ["a.txt"]
        sbx.files.rename("/tmp/d/a.txt", "/tmp/d/b.txt")
        assert sbx.files.exists("/tmp/d/b.txt") and not sbx.files.exists("/tmp/d/a.txt")
        sbx.files.remove("/tmp/d")
        assert not sbx.files.exists("/tmp/d")

        listed = {s.sandbox_id: s for s in Sandbox.list()}
        assert listed[sbx.sandbox_id].metadata == {"user": "u1"}
        assert listed[sbx.sandbox_id].template_id == IMAGE
        again = Sandbox.connect(sbx.sandbox_id)
        assert again.commands.run("echo same").stdout == "same\n"
        assert sbx.is_running()
    assert not sbx.is_running()
    with pytest.raises(NotFoundException):
        Sandbox.connect(sbx.sandbox_id)


def test_get_host_reaches_a_server(daemon: vmlayer.Client) -> None:
    with Sandbox.create(IMAGE) as sbx:
        sbx.commands.run("cd /tmp && echo up > i && python3 -m http.server 8000", background=True)
        sbx.vmlayer.wait_for_port(8000)
        host, port = sbx.get_host(8000).split(":")
        deadline = time.time() + 10
        while True:
            try:
                with socket.create_connection((host, int(port)), timeout=5) as conn:
                    conn.sendall(b"GET /i HTTP/1.0\r\n\r\n")
                    assert b"up" in conn.recv(4096) + conn.recv(4096)
                break
            except ConnectionRefusedError:
                assert time.time() < deadline
                time.sleep(0.2)
        with pytest.raises(NotImplementedError):
            sbx.set_timeout(60)


def test_snapshot_templates(daemon: vmlayer.Client) -> None:
    create_template("e2b-test-tpl", IMAGE, setup="echo ready > /etc/tpl")
    try:
        with Sandbox.create("e2b-test-tpl") as sbx:
            assert sbx.template_id == "e2b-test-tpl"
            assert sbx.commands.run("cat /etc/tpl").stdout == "ready\n"
    finally:
        for snap in daemon.list_snapshots(labels={"e2b.template": "e2b-test-tpl"}):
            snap.delete()
