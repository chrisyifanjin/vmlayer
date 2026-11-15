"""Stress the snapshot / restore / fork paths of a memory-snapshot backend.

A process writes to the workspace volume 100 times a second while the
sandbox is snapshotted, restored and forked repeatedly. Every restored or
forked sandbox must answer within a few seconds, be able to write to its own
volume, and have the writer still running; the parent's writer must keep
running too, and its original process handle must still work at the end.

    cd sdk/python
    uv run python ../../bench/stress_snapshots.py --restores 20 --forks 10 --children 3

Exit status is non-zero if any sandbox misbehaved.
"""

from __future__ import annotations

import argparse
import statistics
import sys
import time
from datetime import timedelta

import vmlayer
from vmlayer import Sandbox

WRITER = (
    "import os, time, itertools\n"
    "for i in itertools.count():\n"
    "    open('/work/n.tmp', 'w').write(str(i))\n"
    "    os.replace('/work/n.tmp', '/work/n')\n"  # atomic: readers never see a torn file
    "    time.sleep(0.01)\n"
)


def counter(sb: Sandbox) -> int:
    return int(sb.files.read_text("/work/n"))


def advancing(sb: Sandbox, pause: float = 0.2) -> bool:
    a = counter(sb)
    time.sleep(pause)
    return counter(sb) > a


def usable(sb: Sandbox) -> bool:
    """New work runs, the volume is writable, and the old writer survived."""
    p = sb.spawn("echo x >> /work/t && test -s /work/t")
    return p.wait(timeout=timedelta(seconds=5)).exit_code == 0 and advancing(sb)


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--image", default="python:3.12-alpine")
    ap.add_argument("--restores", type=int, default=20)
    ap.add_argument("--forks", type=int, default=10)
    ap.add_argument("--children", type=int, default=3)
    args = ap.parse_args()

    client = vmlayer.Client(timeout=timedelta(seconds=15))
    vmlayer.set_default_client(client)
    sb = Sandbox.create(args.image, entrypoint=None)
    if not sb.capabilities.memory_snapshot:
        print(f"backend {sb.backend.value} has no memory snapshots; nothing to stress")
        sb.kill()
        return 0
    failures = 0
    try:
        sb.files.write("/work/writer.py", WRITER)
        writer = sb.spawn("python3 /work/writer.py")
        time.sleep(1)

        times = []
        for i in range(args.restores):
            snap = sb.snapshot()
            t0 = time.perf_counter()
            child = Sandbox.restore(snap)
            ok = False
            try:
                ok = usable(child)
                times.append(time.perf_counter() - t0)
            except Exception as e:
                print(f"restore {i}: {type(e).__name__}: {str(e)[:160]}", flush=True)
            failures += not ok
            child.kill()
            snap.delete()
            if not advancing(sb, 0.1):
                failures += 1
                print(f"restore {i}: the parent's writer stalled", flush=True)
        if times:
            print(
                f"restores: {args.restores - failures}/{args.restores} ok, "
                f"median restore-to-verified {statistics.median(times) * 1000:.0f} ms",
                flush=True,
            )

        before = failures
        times = []
        for i in range(args.forks):
            t0 = time.perf_counter()
            kids = sb.fork(args.children)
            times.append(time.perf_counter() - t0)
            for k in kids:
                ok = False
                try:
                    ok = usable(k)
                except Exception as e:
                    print(f"fork {i}: {type(e).__name__}: {str(e)[:160]}", flush=True)
                failures += not ok
                k.kill()
            if not advancing(sb, 0.1):
                failures += 1
                print(f"fork {i}: the parent's writer stalled", flush=True)
        total = args.forks * args.children
        if times:
            print(
                f"fork children: {total - (failures - before)}/{total} ok, "
                f"median fork({args.children}) {statistics.median(times) * 1000:.0f} ms",
                flush=True,
            )

        writer.kill()
        signal = writer.wait(timeout=timedelta(seconds=10)).signal
        n = args.restores + args.forks
        print(
            f"original process handle after {n} snapshots: killed with signal {signal}", flush=True
        )
        failures += signal != 9
    finally:
        sb.kill()
    print("PASS" if failures == 0 else f"FAIL ({failures} problems)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
