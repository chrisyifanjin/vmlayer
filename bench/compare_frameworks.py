"""Head to head: each agent framework's own sandbox vs. the same framework on vmlayer.

For every framework the two implementations run the same image, the same
operations, on the same machine, driven through the framework's own
interface. Nothing is compared against published numbers.

    cd sdk/python
    uv sync --group dev --group integrations --group bench
    uv run python ../../bench/compare_frameworks.py \
        --vmlayer docker=unix://$HOME/.vmlayer/vmlayerd.sock \
        --vmlayer firecracker=unix://$HOME/.vmlayer/kvm.sock \
        --json compare.json

Scenarios

  mini-swe-agent   `DockerEnvironment` (its default: `docker run -d`, then one
                   `docker exec` subprocess per action) vs `VmlayerEnvironment`.
  AutoGen          `DockerCommandLineCodeExecutor` (docker SDK) vs
                   `VmlayerCodeExecutor`.
  episode reset    The thing neither built-in sandbox can do: start every
                   episode from a prepared state (dependencies installed).
                   Built-in: new container + install each time. vmlayer:
                   install once, snapshot, restore per episode.

All times are wall clock in milliseconds; p50 over the stated repetitions.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import sys
import tempfile
import time
from collections.abc import Callable
from datetime import timedelta
from typing import Any

import vmlayer

IMAGE = "python:3.12-slim"
PREPARE = "pip install -q --root-user-action=ignore --disable-pip-version-check requests"
VERIFY = "python3 -c 'import requests; print(requests.__version__)'"


def p50(xs: list[float]) -> float:
    return statistics.median(xs)


Row = dict[str, Any]


# -- mini-swe-agent ------------------------------------------------------------


def bench_minisweagent(make: Callable[[], Any], runs: int, actions: int) -> Row:
    starts, acts, pys = [], [], []
    for _ in range(runs):
        t0 = time.perf_counter()
        env = make()
        first = env.execute({"command": "true"})
        assert first["returncode"] == 0, first
        starts.append((time.perf_counter() - t0) * 1000)
        try:
            for _ in range(actions):
                t = time.perf_counter()
                out = env.execute({"command": "echo hi"})
                acts.append((time.perf_counter() - t) * 1000)
                assert out["output"].strip() == "hi", out
            for _ in range(max(3, actions // 5)):
                t = time.perf_counter()
                out = env.execute({"command": "python3 -c 'print(6*7)'"})
                pys.append((time.perf_counter() - t) * 1000)
                assert out["output"].strip() == "42", out
        finally:
            env.cleanup()
    return {
        "start to first action": p50(starts),
        "action (echo)": p50(acts),
        "action (python)": p50(pys),
    }


def scenario_minisweagent(
    clients: dict[str, vmlayer.Client], runs: int, actions: int
) -> dict[str, Row]:
    from minisweagent.environments.docker import DockerEnvironment

    from vmlayer.integrations.minisweagent import VmlayerEnvironment

    out: dict[str, Row] = {}
    out["built-in DockerEnvironment"] = bench_minisweagent(
        lambda: DockerEnvironment(image=IMAGE, cwd="/tmp"), runs, actions
    )
    for name, client in clients.items():
        out[f"vmlayer ({name})"] = bench_minisweagent(
            lambda client=client: VmlayerEnvironment(image=IMAGE, cwd="/tmp", client=client),
            runs,
            actions,
        )
    return out


# -- AutoGen -------------------------------------------------------------------


async def bench_autogen(make: Callable[[], Any], runs: int, blocks: int) -> Row:
    from autogen_core import CancellationToken
    from autogen_core.code_executor import CodeBlock

    token = CancellationToken()
    starts, runs_ms, stops = [], [], []
    for _ in range(runs):
        ex = make()
        t0 = time.perf_counter()
        await ex.start()
        r = await ex.execute_code_blocks([CodeBlock(language="python", code="print(6 * 7)")], token)
        assert r.exit_code == 0 and "42" in r.output, r
        starts.append((time.perf_counter() - t0) * 1000)
        for i in range(blocks):
            t = time.perf_counter()
            r = await ex.execute_code_blocks(
                [CodeBlock(language="python", code=f"print({i} + 1)")], token
            )
            runs_ms.append((time.perf_counter() - t) * 1000)
            assert r.exit_code == 0, r
        t = time.perf_counter()
        await ex.stop()
        stops.append((time.perf_counter() - t) * 1000)
    return {
        "start to first result": p50(starts),
        "code block (python)": p50(runs_ms),
        "stop": p50(stops),
    }


def scenario_autogen(clients: dict[str, vmlayer.Client], runs: int, blocks: int) -> dict[str, Row]:
    from autogen_ext.code_executors.docker import DockerCommandLineCodeExecutor

    from vmlayer.integrations.autogen import VmlayerCodeExecutor

    out: dict[str, Row] = {}
    with tempfile.TemporaryDirectory(prefix="autogen-bench-") as tmp:
        out["built-in DockerCommandLineCodeExecutor"] = asyncio.run(
            bench_autogen(
                lambda: DockerCommandLineCodeExecutor(image=IMAGE, work_dir=tmp, timeout=60),
                runs,
                blocks,
            )
        )
        for name, client in clients.items():
            out[f"vmlayer ({name})"] = asyncio.run(
                bench_autogen(
                    lambda client=client: VmlayerCodeExecutor(
                        image=IMAGE, timeout=60, client=client
                    ),
                    runs,
                    blocks,
                )
            )
    return out


# -- episode reset -------------------------------------------------------------


def scenario_episode_reset(clients: dict[str, vmlayer.Client], episodes: int) -> dict[str, Row]:
    from minisweagent.environments.docker import DockerEnvironment

    out: dict[str, Row] = {}

    # Built-in: every episode starts a container and installs again.
    times = []
    for _ in range(episodes):
        t0 = time.perf_counter()
        env = DockerEnvironment(image=IMAGE, cwd="/tmp", timeout=300)
        try:
            assert env.execute({"command": PREPARE})["returncode"] == 0
            assert env.execute({"command": VERIFY})["returncode"] == 0
            times.append((time.perf_counter() - t0) * 1000)
        finally:
            env.cleanup()
    out["built-in Docker: new container + install"] = {
        "per episode": p50(times),
        "one-time preparation": 0.0,
    }

    # vmlayer: install once, snapshot, restore per episode.
    for name, client in clients.items():
        t0 = time.perf_counter()
        base = vmlayer.Sandbox.create(
            IMAGE, entrypoint=None, timeout=timedelta(minutes=30), client=client
        )
        try:
            assert base.exec(PREPARE, timeout=timedelta(minutes=5)).ok
            snap = base.snapshot()
            prepare = (time.perf_counter() - t0) * 1000
            times = []
            for _ in range(episodes):
                t1 = time.perf_counter()
                sb = vmlayer.Sandbox.restore(snap, client=client)
                try:
                    assert sb.exec(VERIFY).ok
                    times.append((time.perf_counter() - t1) * 1000)
                finally:
                    sb.kill()
            kind = "memory+disk" if snap.includes_memory else "disk"
            out[f"vmlayer ({name}): restore {kind} snapshot"] = {
                "per episode": p50(times),
                "one-time preparation": prepare,
            }
            snap.delete()
        finally:
            base.kill()
    return out


# -- output --------------------------------------------------------------------


def table(title: str, rows: dict[str, Row]) -> str:
    cols = list(next(iter(rows.values())).keys())
    lines = [f"### {title}", "", "| | " + " | ".join(cols) + " |", "|---|" + "---:|" * len(cols)]
    for name, row in rows.items():
        lines.append(f"| {name} | " + " | ".join(f"{row[c]:,.0f} ms" for c in cols) + " |")
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--vmlayer",
        action="append",
        default=[],
        metavar="NAME=ENDPOINT",
        help="a vmlayer daemon to test (repeatable)",
    )
    ap.add_argument("--runs", type=int, default=5, help="sandbox lifecycles per implementation")
    ap.add_argument("--actions", type=int, default=30, help="actions per lifecycle")
    ap.add_argument("--episodes", type=int, default=5)
    ap.add_argument("--only", choices=["minisweagent", "autogen", "episode"], action="append")
    ap.add_argument("--json", default="")
    args = ap.parse_args()

    clients: dict[str, vmlayer.Client] = {}
    for spec in args.vmlayer:
        name, _, endpoint = spec.partition("=")
        client = vmlayer.Client(endpoint, timeout=timedelta(seconds=60))
        client.pull_image(IMAGE)
        clients[name] = client
    if not clients:
        print("pass at least one --vmlayer NAME=ENDPOINT", file=sys.stderr)
        return 2

    results: dict[str, dict[str, Row]] = {}
    want = args.only or ["minisweagent", "autogen", "episode"]
    if "minisweagent" in want:
        results["mini-swe-agent: environment"] = scenario_minisweagent(
            clients, args.runs, args.actions
        )
        print(
            table("mini-swe-agent: environment", results["mini-swe-agent: environment"]), flush=True
        )
    if "autogen" in want:
        results["AutoGen: code executor"] = scenario_autogen(clients, args.runs, args.actions)
        print(table("AutoGen: code executor", results["AutoGen: code executor"]), flush=True)
    if "episode" in want:
        results["Episode reset from a prepared state"] = scenario_episode_reset(
            clients, args.episodes
        )
        print(
            table(
                "Episode reset from a prepared state",
                results["Episode reset from a prepared state"],
            ),
            flush=True,
        )
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"image": IMAGE, "results_ms": results}, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
