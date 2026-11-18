"""Harbor trials on vmlayer sandboxes, through Harbor's own CLI.

Needs Python 3.12+ and Harbor; CI runs it with
``uv run --python 3.12 --with harbor==0.26.0 ... pytest tests/test_harbor.py``.
Skipped elsewhere.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

import vmlayer

pytest.importorskip("harbor", reason="Harbor is not installed (Python 3.12+)")

IMAGE = "python:3.12-slim"
ENV = "vmlayer.integrations.harbor:VmlayerEnvironment"


def make_task(root: Path, name: str, *, solve: str, network_mode: str = "public") -> Path:
    """A task in Harbor's layout: write hello.txt in /app; the verifier checks it."""
    task = root / name
    (task / "environment").mkdir(parents=True)
    (task / "solution").mkdir()
    (task / "tests").mkdir()
    (task / "task.toml").write_text(
        f"""schema_version = "1.1"

[task]
name = "vmlayer/{name}"

[verifier]
timeout_sec = 120.0

[agent]
timeout_sec = 120.0

[environment]
docker_image = "{IMAGE}"
workdir = "/app"
network_mode = "{network_mode}"
cpus = 1
memory_mb = 1024
"""
    )
    (task / "instruction.md").write_text('Create /app/hello.txt containing "Hello, world!".\n')
    (task / "solution" / "solve.sh").write_text(f"#!/bin/bash\n{solve}\n")
    (task / "tests" / "test.sh").write_text(
        "#!/bin/bash\n"
        'if [ "$(cat /app/hello.txt)" = "Hello, world!" ]; then r=1; else r=0; fi\n'
        "echo $r > /logs/verifier/reward.txt\n"
    )
    return task


def run_trial(daemon: vmlayer.Client, task: Path, agent: str, jobs: Path) -> dict:  # type: ignore[type-arg]
    harbor = shutil.which("harbor", path=str(Path(sys.executable).parent)) or "harbor"
    out = subprocess.run(
        [harbor, "run", "-p", str(task), "-a", agent, "-e", ENV,
         "--ek", f"endpoint={daemon.endpoint}", "-o", str(jobs)],
        capture_output=True, text=True, timeout=600,
    )  # fmt: skip
    results = sorted(jobs.glob(f"*/{task.name}__*/result.json"))
    assert results, f"no trial result\n{out.stdout[-3000:]}\n{out.stderr[-3000:]}"
    result: dict = json.loads(results[-1].read_text())  # type: ignore[type-arg]
    assert not result.get("exception_info"), result["exception_info"]
    return result


def reward(result: dict) -> float:  # type: ignore[type-arg]
    return float(result["verifier_result"]["rewards"]["reward"])


def test_oracle_solves_and_nop_fails(daemon: vmlayer.Client, tmp_path: Path) -> None:
    task = make_task(tmp_path, "hello", solve='echo "Hello, world!" > hello.txt')
    assert reward(run_trial(daemon, task, "oracle", tmp_path / "jobs-oracle")) == 1.0
    assert reward(run_trial(daemon, task, "nop", tmp_path / "jobs-nop")) == 0.0
    # Trials leave no sandboxes behind.
    assert not [s for s in daemon.list_sandboxes() if "harbor.session" in s.labels]


def test_no_network_tasks_run_without_a_network(daemon: vmlayer.Client, tmp_path: Path) -> None:
    probe = (
        "if python3 -c \"import socket; socket.create_connection(('1.1.1.1', 80), timeout=3)\""
        ' 2>/dev/null; then echo reachable > hello.txt; else echo "Hello, world!" > hello.txt; fi'
    )
    task = make_task(tmp_path, "offline", solve=probe, network_mode="no-network")
    assert reward(run_trial(daemon, task, "oracle", tmp_path / "jobs")) == 1.0
