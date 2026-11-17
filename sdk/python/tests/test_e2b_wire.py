"""The unmodified E2B SDK against vmlayer's E2B gateway.

Nothing here imports vmlayer's own E2B facade: the code under test is
``from e2b import Sandbox`` with ``E2B_API_URL`` and ``E2B_SANDBOX_URL``
pointing at ``vmlayer.e2b_server``.
"""

from __future__ import annotations

import os
import socket
import threading
import time
from collections.abc import Iterator

import pytest

import vmlayer

e2b = pytest.importorskip("e2b")
uvicorn = pytest.importorskip("uvicorn")

from vmlayer.e2b_server import Gateway, build_app  # noqa: E402

IMAGE = "python:3.12-alpine"
KEY = "e2b_test_key_0123456789"


@pytest.fixture(scope="module")
def gateway(daemon: vmlayer.Client) -> Iterator[str]:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    gw = Gateway(daemon, api_key=KEY, base_image=IMAGE)
    server = uvicorn.Server(
        uvicorn.Config(build_app(gw), host="127.0.0.1", port=port, log_level="warning")
    )
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{port}"
    deadline = time.time() + 10
    while not server.started:
        assert time.time() < deadline
        time.sleep(0.05)
    saved = {
        k: os.environ.get(k) for k in ("E2B_API_URL", "E2B_SANDBOX_URL", "E2B_API_KEY", "E2B_DEBUG")
    }
    os.environ.update({"E2B_API_URL": url, "E2B_SANDBOX_URL": url, "E2B_API_KEY": KEY})
    os.environ.pop("E2B_DEBUG", None)
    try:
        yield url
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        server.should_exit = True
        thread.join(timeout=10)


def test_unmodified_e2b_sdk(gateway: str) -> None:
    from e2b import Sandbox
    from e2b.sandbox.commands.command_handle import CommandExitException

    sbx = Sandbox.create(IMAGE, timeout=120, metadata={"user": "u1"}, envs={"MODE": "wire"})
    try:
        r = sbx.commands.run("echo $MODE; echo err >&2")
        assert (r.stdout, r.stderr, r.exit_code) == ("wire\n", "err\n", 0)
        assert sbx.commands.run("pwd", cwd="/tmp").stdout == "/tmp\n"
        with pytest.raises(CommandExitException) as caught:
            sbx.commands.run("echo partial; exit 3")
        assert caught.value.exit_code == 3

        seen: list[str] = []
        sbx.commands.run("echo streamed", on_stdout=seen.append)
        assert "".join(seen) == "streamed\n"

        handle = sbx.commands.run("sleep 60", background=True)
        assert handle.pid in [p.pid for p in sbx.commands.list()]
        assert sbx.commands.kill(handle.pid)

        sbx.files.write("/tmp/d/hello.txt", "hi there")
        assert sbx.files.read("/tmp/d/hello.txt") == "hi there"
        assert [e.name for e in sbx.files.list("/tmp/d")] == ["hello.txt"]
        assert sbx.files.exists("/tmp/d/hello.txt")
        sbx.files.rename("/tmp/d/hello.txt", "/tmp/d/bye.txt")
        assert not sbx.files.exists("/tmp/d/hello.txt")
        assert sbx.files.make_dir("/tmp/new")
        assert not sbx.files.make_dir("/tmp/new")
        sbx.files.remove("/tmp/d")
        assert not sbx.files.exists("/tmp/d")

        listed = {s.sandbox_id: s for s in Sandbox.list().next_items()}
        assert listed[sbx.sandbox_id].metadata == {"user": "u1"}
        sbx.set_timeout(300)
        again = Sandbox.connect(sbx.sandbox_id)
        assert again.commands.run("echo same").stdout == "same\n"
        assert sbx.is_running()
    finally:
        sbx.kill()
    assert not sbx.is_running()


def test_api_key_is_required(gateway: str) -> None:
    from e2b import Sandbox

    with pytest.raises(Exception, match=r"(?i)auth|401|key"):
        Sandbox.create(IMAGE, api_key="wrong")


def test_unmodified_async_e2b_sdk(gateway: str) -> None:
    import asyncio

    from e2b import AsyncSandbox

    async def scenario() -> None:
        sbx = await AsyncSandbox.create(IMAGE, timeout=60)
        try:
            r = await sbx.commands.run("python3 -c 'print(6 * 7)'")
            assert r.stdout == "42\n"
            await sbx.files.write("/tmp/a.txt", "async")
            assert await sbx.files.read("/tmp/a.txt") == "async"
        finally:
            await sbx.kill()

    asyncio.run(scenario())
