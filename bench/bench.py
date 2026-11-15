"""vmlayer benchmark: latency of the operations an agent runtime calls.

Runs against whatever daemon VMLAYER_ENDPOINT points at and reports
p50/p95/min in milliseconds, as a table and as JSON.

    cd sdk/python
    uv run python ../../bench/bench.py --image python:3.12-alpine --runs 20
    uv run python ../../bench/bench.py --json out.json --label "firecracker, nested KVM on M5"

What is measured (wall clock, from the client):

  create          Sandbox.create() returns (image already cached)
  create+exec     create, then the first `true` finishes: time to interactive
  exec            one `true` round trip in a running sandbox
  exec python     `python3 -c pass`: what a code-running agent pays per step
  file write/read a 1 MiB file each way
  snapshot        Sandbox.snapshot() returns (sandbox running again)
  restore+exec    Sandbox.restore(), then the first `true` finishes
  fork(N)         Sandbox.fork(N) returns N running children (memory backends)
  vol snapshot    Volume.snapshot() of the workspace
  kill            Sandbox.kill() returns
  parallel create C sandboxes created concurrently, total wall time
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import platform
import statistics
import sys
import time
from collections.abc import Callable
from datetime import timedelta
from typing import Any

import vmlayer
from vmlayer import Sandbox


def ms(fn: Callable[[], Any]) -> tuple[float, Any]:
    t = time.perf_counter()
    out = fn()
    return (time.perf_counter() - t) * 1000.0, out


def summarize(samples: list[float]) -> dict[str, float]:
    s = sorted(samples)
    p95 = s[min(len(s) - 1, max(0, round(0.95 * (len(s) - 1))))]
    return {"n": len(s), "p50": statistics.median(s), "p95": p95, "min": s[0], "max": s[-1]}


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--image", default="python:3.12-alpine")
    ap.add_argument("--backend", default="auto")
    ap.add_argument("--runs", type=int, default=20, help="iterations for lifecycle operations")
    ap.add_argument("--exec-runs", type=int, default=200, help="iterations for exec round trips")
    ap.add_argument("--fork", type=int, default=8, help="children per fork (0 disables)")
    ap.add_argument(
        "--parallel", type=int, default=8, help="sandboxes for the parallel create test"
    )
    ap.add_argument("--memory-mb", type=int, default=256)
    ap.add_argument("--label", default="")
    ap.add_argument("--json", default="")
    args = ap.parse_args()

    client = vmlayer.default_client()
    info = client.info()
    backend = vmlayer.Backend(args.backend)
    client.pull_image(args.image, backend=backend)  # keep image time out of the numbers
    results: dict[str, dict[str, float]] = {}
    create_kw: dict[str, Any] = dict(
        backend=backend, memory_mb=args.memory_mb, entrypoint=None, timeout=timedelta(minutes=10)
    )

    def record(name: str, samples: list[float]) -> None:
        if samples:
            results[name] = summarize(samples)
            r = results[name]
            print(
                f"  {name:22s} p50 {r['p50']:8.1f}  p95 {r['p95']:8.1f}"
                f"  min {r['min']:8.1f}  (n={r['n']})",
                flush=True,
            )

    probe = Sandbox.create(args.image, **create_kw)
    caps = probe.capabilities
    actual_backend = probe.backend.value
    engine = next((b.engine_version for b in info.backends if b.name == actual_backend), "")
    probe.kill()
    print(
        f"vmlayer {info.daemon_version}  backend {actual_backend} {engine}"
        f"  image {args.image}  {args.label}",
        flush=True,
    )

    # -- lifecycle -----------------------------------------------------------
    creates, tti, kills = [], [], []
    for _ in range(args.runs):
        t0 = time.perf_counter()
        sb = Sandbox.create(args.image, **create_kw)
        try:
            t1 = time.perf_counter()
            sb.exec("true")
            t2 = time.perf_counter()
            creates.append((t1 - t0) * 1000)
            tti.append((t2 - t0) * 1000)
        finally:
            kills.append(ms(sb.kill)[0])
    record("create", creates)
    record("create+exec", tti)
    record("kill", kills)

    # -- inside one sandbox -----------------------------------------------------
    sb = Sandbox.create(args.image, **create_kw)
    try:
        record("exec", [ms(lambda: sb.exec("true"))[0] for _ in range(args.exec_runs)])
        record(
            "exec python",
            [
                ms(lambda: sb.exec("python3 -c pass"))[0]
                for _ in range(max(10, args.exec_runs // 10))
            ],
        )
        blob = b"x" * (1 << 20)
        record(
            "file write 1MiB",
            [ms(lambda: sb.files.write("/work/blob", blob))[0] for _ in range(args.runs)],
        )
        record(
            "file read 1MiB", [ms(lambda: sb.files.read("/work/blob"))[0] for _ in range(args.runs)]
        )
        ws = sb.workspace
        if ws is not None:
            vsnaps = []
            times = []
            for _ in range(min(args.runs, 10)):
                t, v = ms(ws.snapshot)
                times.append(t)
                vsnaps.append(v)
            record("vol snapshot", times)
            for v in vsnaps:
                v.delete()

        # -- snapshot / restore / fork ---------------------------------------------
        if caps.snapshot:
            snaps, snap_times = [], []
            for _ in range(min(args.runs, 10)):
                t, s = ms(sb.snapshot)
                snap_times.append(t)
                snaps.append(s)
            kind = "memory" if caps.memory_snapshot else "fs"
            record(f"snapshot ({kind})", snap_times)
            restores = []
            for _ in range(min(args.runs, 10)):
                t0 = time.perf_counter()
                r = Sandbox.restore(snaps[-1], timeout=timedelta(minutes=10))
                try:
                    r.exec("true")
                    restores.append((time.perf_counter() - t0) * 1000)
                finally:
                    r.kill()
            record("restore+exec", restores)
            results["_snapshot_size_mib"] = {"p50": snaps[-1].size_bytes / (1 << 20)}
            for s in snaps:
                s.delete()
        if caps.fork and args.fork > 0:
            totals = []
            for _ in range(min(args.runs, 5)):
                t, kids = ms(lambda: sb.fork(args.fork))
                totals.append(t)
                for k in kids:
                    k.kill()
            record(f"fork({args.fork})", totals)
            results[f"fork({args.fork}) per child"] = summarize([t / args.fork for t in totals])
    finally:
        sb.kill()

    # -- parallel create ------------------------------------------------------------
    if args.parallel > 1:
        totals = []
        for _ in range(3):
            t0 = time.perf_counter()
            boxes = []
            try:
                with concurrent.futures.ThreadPoolExecutor(args.parallel) as pool:
                    futures = [
                        pool.submit(Sandbox.create, args.image, **create_kw)
                        for _ in range(args.parallel)
                    ]
                    first_error = None
                    for future in concurrent.futures.as_completed(futures):
                        try:
                            boxes.append(future.result())
                        except Exception as exc:
                            if first_error is None:
                                first_error = exc
                if first_error is not None:
                    raise first_error
                totals.append((time.perf_counter() - t0) * 1000)
            finally:
                with concurrent.futures.ThreadPoolExecutor(args.parallel) as pool:
                    list(pool.map(lambda b: b.kill(), boxes))
        record(f"parallel create x{args.parallel}", totals)

    if args.json:
        doc = {
            "label": args.label,
            "daemon_version": info.daemon_version,
            "backend": actual_backend,
            "engine_version": engine,
            "image": args.image,
            "memory_mb": args.memory_mb,
            "client": f"{platform.system()} {platform.machine()}",
            "results_ms": results,
        }
        with open(args.json, "w") as f:
            json.dump(doc, f, indent=2)
        print(f"wrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
