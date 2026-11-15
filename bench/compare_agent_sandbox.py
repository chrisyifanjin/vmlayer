"""Compare host-managed vmlayer VMs with Agent Sandbox pods under gVisor.

Both sides run the same pre-pulled ARM64 OCI image on one physical host.
Kubernetes runs in Minikube; vmlayerd runs in a Lima VM with nested KVM.
Each has the same CPU and RAM limit. This is a local, directional comparison:
the two Linux hosts, isolation mechanisms and client transports differ.

    VMLAYER_ENDPOINT=unix:///path/to/kvm.sock \
      python bench/compare_agent_sandbox.py --transport api \
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
from concurrent.futures import ThreadPoolExecutor
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
    parser.add_argument("--transport", choices=("kubectl", "api"), default="kubectl")
    parser.add_argument("--context", required=True)
    parser.add_argument("--image", default="python:3.12-alpine")
    parser.add_argument(
        "--no-workspace", action="store_true", help="use the VM root disk instead of a volume"
    )
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--parallel", type=int, default=1, help="cold sandboxes per batch")
    parser.add_argument("--json", required=True)
    args = parser.parse_args()
    if args.runs < 1 or args.parallel < 1:
        parser.error("--runs and --parallel must be positive")

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

    if args.transport == "api":
        from kubernetes import client as kube_client
        from kubernetes import config as kube_config
        from kubernetes import watch as kube_watch
        from kubernetes.client.exceptions import ApiException
        from kubernetes.stream import stream as kube_stream

        kube_config.load_kube_config(context=args.context)

    def k8s_create(manifest: dict[str, Any]) -> None:
        name = manifest["metadata"]["name"]
        if args.transport == "kubectl":
            kubectl("apply", "-f", "-", stdin=json.dumps(manifest))
            kubectl("wait", "--for=condition=Ready", f"sandbox/{name}", "--timeout=120s")
            return
        custom = kube_client.CustomObjectsApi()
        created = custom.create_namespaced_custom_object(
            "agents.x-k8s.io", "v1beta1", "default", "sandboxes", manifest
        )
        events = kube_watch.Watch()
        try:
            for event in events.stream(
                custom.list_namespaced_custom_object,
                "agents.x-k8s.io",
                "v1beta1",
                "default",
                "sandboxes",
                field_selector=f"metadata.name={name}",
                resource_version=created["metadata"]["resourceVersion"],
                timeout_seconds=120,
            ):
                conditions = event["object"].get("status", {}).get("conditions", [])
                if any(c.get("type") == "Ready" and c.get("status") == "True" for c in conditions):
                    return
        finally:
            events.stop()
        raise TimeoutError(f"Sandbox {name} did not become Ready")

    def k8s_exec(name: str, *argv: str) -> str:
        if args.transport == "kubectl":
            return kubectl("exec", f"pod/{name}", "--", *argv)
        core = kube_client.CoreV1Api()
        return kube_stream(
            core.connect_get_namespaced_pod_exec,
            name,
            "default",
            command=list(argv),
            stderr=True,
            stdin=False,
            stdout=True,
            tty=False,
        ).strip()

    def k8s_delete(name: str) -> None:
        if args.transport == "kubectl":
            kubectl("delete", "sandbox", name, "--wait=true", "--ignore-not-found")
            kubectl("wait", "--for=delete", f"pod/{name}", "--timeout=60s")
            return
        custom = kube_client.CustomObjectsApi()
        core = kube_client.CoreV1Api()
        custom.delete_namespaced_custom_object(
            "agents.x-k8s.io", "v1beta1", "default", "sandboxes", name, body={}
        )
        deadline = time.monotonic() + 60
        while True:
            try:
                core.read_namespaced_pod(name, "default")
            except ApiException as exc:
                if exc.status == 404:
                    return
                raise
            if time.monotonic() >= deadline:
                raise TimeoutError(f"Pod {name} was not deleted")
            time.sleep(0.1)

    client = vmlayer.default_client()
    client.pull_image(args.image, backend=vmlayer.Backend.FIRECRACKER)
    samples: dict[str, dict[str, list[float]]] = {
        name: {
            "create": [],
            "create+python": [],
            "python first": [],
            "python repeat": [],
            "exec true": [],
            "delete": [],
        }
        for name in ("agent-sandbox/gvisor", "vmlayer/firecracker")
    }
    kernels: dict[str, str] = {}
    batch_completion: dict[str, list[float]] = {
        "agent-sandbox/gvisor": [],
        "vmlayer/firecracker": [],
    }

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
            k8s_create(manifest)
            create_ms = (time.perf_counter() - start) * 1000
            python_ms, output = timed(lambda: k8s_exec(name, "python3", "-c", "print(6*7)"))
            assert output == "42", output
            create_python_ms = (time.perf_counter() - start) * 1000
            repeat_ms, output = timed(lambda: k8s_exec(name, "python3", "-c", "print(6*7)"))
            assert output == "42", output
            true_ms, _ = timed(lambda: k8s_exec(name, "true"))
            kernels["agent-sandbox/gvisor"] = k8s_exec(name, "uname", "-r")
            if measure:
                row["create"].append(create_ms)
                row["create+python"].append(create_python_ms)
                row["python first"].append(python_ms)
                row["python repeat"].append(repeat_ms)
                row["exec true"].append(true_ms)
        finally:
            start = time.perf_counter()
            k8s_delete(name)
            if measure:
                row["delete"].append((time.perf_counter() - start) * 1000)

    def vmlayer_once(measure: bool) -> None:
        row = samples["vmlayer/firecracker"]
        create_ms, sb = timed(
            lambda: vmlayer.Sandbox.create(
                args.image,
                backend=vmlayer.Backend.FIRECRACKER,
                memory_mb=256,
                workspace=None if args.no_workspace else vmlayer.AUTO,
                entrypoint=["sleep", "3600"],
                client=client,
            )
        )
        try:
            python_ms, result = timed(lambda: sb.exec("python3 -c 'print(6*7)'"))
            assert result.exit_code == 0 and result.stdout.strip() == "42"
            create_python_ms = create_ms + python_ms
            repeat_ms, result = timed(lambda: sb.exec("python3 -c 'print(6*7)'"))
            assert result.exit_code == 0 and result.stdout.strip() == "42"
            true_ms, result = timed(lambda: sb.exec("true"))
            assert result.exit_code == 0
            kernels["vmlayer/firecracker"] = sb.exec("uname -r").stdout.strip()
            if measure:
                row["create"].append(create_ms)
                row["create+python"].append(create_python_ms)
                row["python first"].append(python_ms)
                row["python repeat"].append(repeat_ms)
                row["exec true"].append(true_ms)
        finally:
            delete_ms, _ = timed(sb.kill)
            if measure:
                row["delete"].append(delete_ms)

    # Pull and prepare on both hosts before timing. Alternate order to reduce
    # drift from background work and host temperature.
    k8s_once(False)
    vmlayer_once(False)
    for index in range(args.runs):
        paths = [("agent-sandbox/gvisor", k8s_once), ("vmlayer/firecracker", vmlayer_once)]
        for name, run in paths if index % 2 == 0 else reversed(paths):
            start_at = len(samples[name]["create+python"])
            if args.parallel == 1:
                run(True)
            else:
                with ThreadPoolExecutor(max_workers=args.parallel) as pool:
                    futures = [pool.submit(run, True) for _ in range(args.parallel)]
                    for future in futures:
                        future.result()
            batch_completion[name].append(max(samples[name]["create+python"][start_at:]))
        print(f"completed {index + 1}/{args.runs}", flush=True)

    doc = {
        "image": args.image,
        "context": args.context,
        "transport": args.transport,
        "parallel": args.parallel,
        "workspace": "none" if args.no_workspace else "anonymous-volume",
        "kernels": kernels,
        "samples_ms": samples,
        "batch_completion_ms": {
            backend: summary(values) for backend, values in batch_completion.items()
        },
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
