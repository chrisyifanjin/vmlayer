"""The asyncio client against the live daemon (conftest)."""

from __future__ import annotations

import asyncio
from datetime import timedelta
from pathlib import Path

import pytest

import vmlayer
from vmlayer.aio import AsyncClient, AsyncSandbox

ALPINE = "alpine:3.20"


def run(coro):  # type: ignore[no-untyped-def]
    return asyncio.run(coro)


def test_lifecycle_exec_files_and_processes(daemon: vmlayer.Client, tmp_path: Path) -> None:
    async def body() -> None:
        async with AsyncClient(daemon.endpoint) as client:
            assert await client.health()
            assert (await client.info()).daemon_version
            async with await AsyncSandbox.create(ALPINE, client=client) as sb:
                r = await sb.exec("echo out; echo err >&2; exit 3")
                assert (r.exit_code, r.stdout, r.stderr) == (3, "out\n", "err\n")
                with pytest.raises(vmlayer.ExecTimeout):
                    await sb.exec("sleep 5", timeout=timedelta(seconds=1))

                # Many commands in flight on one channel, none blocking the loop.
                results = await asyncio.gather(*(sb.exec(f"echo {i}") for i in range(8)))
                assert [x.stdout.strip() for x in results] == [str(i) for i in range(8)]

                await sb.files.write("/work/a/b.txt", "hello")
                assert await sb.files.read_text("/work/a/b.txt") == "hello"
                assert [f.name for f in await sb.files.list("/work/a")] == ["b.txt"]
                assert await sb.files.exists("/work/a/b.txt")
                assert not await sb.files.exists("/work/nope")
                src = tmp_path / "tree"
                (src / "sub").mkdir(parents=True)
                (src / "sub" / "f.txt").write_text("deep")
                await sb.files.upload(src, "/work/tree")
                await sb.files.download("/work/tree", tmp_path / "back")
                assert (tmp_path / "back" / "sub" / "f.txt").read_text() == "deep"

                proc = await sb.spawn("for i in 1 2 3; do echo line$i; sleep 0.1; done")
                seen = "".join([c.text async for c in proc])
                assert seen == "line1\nline2\nline3\n"
                assert (await proc.wait()).stdout == seen

                again = await AsyncSandbox.connect(sb.id, client=client)
                assert again == sb and again.image == ALPINE
                assert sb.id in {s.id for s in await client.list_sandboxes()}
            assert sb.id not in {s.id for s in await client.list_sandboxes()}

    run(body())


def test_snapshot_and_restore(daemon: vmlayer.Client) -> None:
    async def body() -> None:
        async with AsyncClient(daemon.endpoint) as client:
            sb = await AsyncSandbox.create(ALPINE, client=client)
            try:
                if not sb.capabilities.snapshot:
                    pytest.skip("backend cannot snapshot")
                await sb.exec("echo kept > /work/f")
                snap = await sb.snapshot()
                async with await AsyncSandbox.restore(snap, client=client) as restored:
                    assert (await restored.exec("cat /work/f")).stdout == "kept\n"
                await client.delete_snapshot(snap)
            finally:
                await sb.kill()

    run(body())


def test_errors_are_the_sync_sdks(daemon: vmlayer.Client) -> None:
    async def body() -> None:
        async with AsyncClient(daemon.endpoint) as client:
            with pytest.raises(vmlayer.SandboxNotFound):
                await AsyncSandbox.connect("sb-does-not-exist", client=client)
        with pytest.raises(vmlayer.DaemonUnavailable):
            async with AsyncClient("unix:///nonexistent/vmlayerd.sock") as gone:
                await gone.health()

    run(body())
