"""Compare host-managed vmlayer VMs with Agent Sandbox pods under gVisor.

Both sides run the same pre-pulled ARM64 OCI image on one physical host.
Kubernetes runs in Minikube; vmlayerd runs in a Lima VM with nested KVM.
Each has the same CPU and RAM limit. This is a local, directional comparison:
the two Linux hosts, isolation mechanisms and client transports differ.

    VMLAYER_ENDPOINT=unix:///path/to/kvm.sock \
      python bench/compare_agent_sandbox.py --kubectl /path/to/kubectl \
      --context vmlayer-compare --runs 10 --json bench/results/compare.json
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import time
import uuid
from collections.abc import Callable
from typing import Any

import vmlayer


def timed(fn: Callable[[], Any]) -> tuple[float, Any]:
    start = time.perf_counter()
    value = fn()
    return (time.perf_counter() - start) * 1000, value


def summary(values: list[float]) -> dict[str, float]:
    ordered = sorted(values)
    return {
        "n": len(ordered),
        "p50": statistics.median(ordered),
        "p95": ordered[min(len(ordered) - 1, round(0.95 * (len(ordered) - 1)))],
        "min": ordered[0],
        "max": ordered[-1],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubectl", default="kubectl")
    parser.add_argument("--context", required=True)
    parser.add_argument("--image", default="python:3.12-alpine")
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--json", required=True)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")

    def kubectl(*argv: str, stdin: str | None = None) -> str:
        result = subprocess.run(
            [args.kubectl, "--context", args.context, *argv],
            input=stdin,
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode:
            raise RuntimeError(f"kubectl {' '.join(argv)}: {result.stderr.strip()}")
        return result.stdout.strip()

    client = vmlayer.default_client()
    client.pull_image(args.image, backend=vmlayer.Backend.FIRECRACKER)
    samples: dict[str, dict[str, list[float]]] = {
        name: {"create": [], "exec true": [], "python first": [], "python repeat": [], "delete": []}
        for name in ("agent-sandbox/gvisor", "vmlayer/firecracker")
    }
    kernels: dict[str, str] = {}

    def k8s_once(measure: bool) -> None:
        name = f"vmlayer-bench-{uuid.uuid4().hex[:12]}"
        manifest = {
            "apiVersion": "agents.x-k8s.io/v1beta1",
            "kind": "Sandbox",
            "metadata": {"name": name},
            "spec": {
                "podTemplate": {
                    "spec": {
                        "runtimeClassName": "gvisor",
                        "containers": [
                            {
                                "name": "runner",
                                "image": args.image,
                                "command": ["sleep", "3600"],
                                "resources": {
                                    "requests": {"cpu": "1", "memory": "256Mi"},
                                    "limits": {"cpu": "1", "memory": "256Mi"},
                                },
                            }
                        ],
                    }
                }
            },
        }
        row = samples["agent-sandbox/gvisor"]
        try:
            start = time.perf_counter()
            kubectl("apply", "-f", "-", stdin=json.dumps(manifest))
            kubectl("wait", "--for=condition=Ready", f"sandbox/{name}", "--timeout=120s")
            create_ms = (time.perf_counter() - start) * 1000
            true_ms, _ = timed(lambda: kubectl("exec", f"pod/{name}", "--", "true"))
            python_ms, output = timed(
                lambda: kubectl("exec", f"pod/{name}", "--", "python3", "-c", "print(6*7)")
            )
            assert output == "42", output
            repeat_ms, output = timed(
                lambda: kubectl("exec", f"pod/{name}", "--", "python3", "-c", "print(6*7)")
            )
            assert output == "42", output
            kernels["agent-sandbox/gvisor"] = kubectl("exec", f"pod/{name}", "--", "uname", "-r")
            if measure:
                row["create"].append(create_ms)
                row["exec true"].append(true_ms)
                row["python first"].append(python_ms)
                row["python repeat"].append(repeat_ms)
        finally:
            start = time.perf_counter()
            kubectl("delete", "sandbox", name, "--wait=true", "--ignore-not-found")
            kubectl("wait", "--for=delete", f"pod/{name}", "--timeout=60s")
            if measure:
                row["delete"].append((time.perf_counter() - start) * 1000)

    def vmlayer_once(measure: bool) -> None:
        row = samples["vmlayer/firecracker"]
        create_ms, sb = timed(
            lambda: vmlayer.Sandbox.create(
                args.image,
                backend=vmlayer.Backend.FIRECRACKER,
                memory_mb=256,
                entrypoint=["sleep", "3600"],
                client=client,
            )
        )
        try:
            true_ms, result = timed(lambda: sb.exec("true"))
            assert result.exit_code == 0
            python_ms, result = timed(lambda: sb.exec("python3 -c 'print(6*7)'"))
            assert result.exit_code == 0 and result.stdout.strip() == "42"
            repeat_ms, result = timed(lambda: sb.exec("python3 -c 'print(6*7)'"))
            assert result.exit_code == 0 and result.stdout.strip() == "42"
            kernels["vmlayer/firecracker"] = sb.exec("uname -r").stdout.strip()
            if measure:
                row["create"].append(create_ms)
                row["exec true"].append(true_ms)
                row["python first"].append(python_ms)
                row["python repeat"].append(repeat_ms)
        finally:
            delete_ms, _ = timed(sb.kill)
            if measure:
                row["delete"].append(delete_ms)

    # Pull and prepare on both hosts before timing. Alternate order to reduce
    # drift from background work and host temperature.
    k8s_once(False)
    vmlayer_once(False)
    for index in range(args.runs):
        for run in (k8s_once, vmlayer_once) if index % 2 == 0 else (vmlayer_once, k8s_once):
            run(True)
        print(f"completed {index + 1}/{args.runs}", flush=True)

    doc = {
        "image": args.image,
        "context": args.context,
        "kernels": kernels,
        "results_ms": {
            backend: {operation: summary(values) for operation, values in operations.items()}
            for backend, operations in samples.items()
        },
    }
    with open(args.json, "w") as output:
        json.dump(doc, output, indent=2)
    for backend, operations in doc["results_ms"].items():
        print(backend)
        for operation, values in operations.items():
            print(f"  {operation:12s} p50 {values['p50']:8.1f}  p95 {values['p95']:8.1f}")
    print(f"wrote {args.json}")


if __name__ == "__main__":
    main()
