"""vmlayer: the layer between agent runtimes and microVMs.

Step 0 of the project: this package is the API *surface* only. Every method
raises ``NotImplementedError``. The point is to agree on the shape before any
backend exists.

Quick tour::

    from vmlayer import Sandbox

    with Sandbox.create("python:3.12", memory_mb=512) as sb:
        result = sb.exec("python -c 'print(40 + 2)'")
        print(result.stdout)                 # "42\\n"

        sb.files.write("/work/main.py", b"print('hi')")
        proc = sb.spawn("python /work/main.py")
        for chunk in proc:
            print(chunk.data.decode(), end="")

        snap = sb.snapshot()                 # freeze memory + disk
        children = sb.fork(4)                # four copies, same state

    sb2 = Sandbox.restore(snap)              # ms-scale cold start
"""

from .api import (
    AsyncSandbox,
    Backend,
    BackendUnsupported,
    Capabilities,
    Client,
    Endpoint,
    ExecResult,
    ExecTimeout,
    FileInfo,
    Files,
    OutputChunk,
    Process,
    Pty,
    PtySize,
    Sandbox,
    SandboxNotFound,
    SandboxState,
    Snapshot,
    SnapshotIncompatible,
    VmlayerError,
)

__all__ = [
    "AsyncSandbox",
    "Backend",
    "BackendUnsupported",
    "Capabilities",
    "Client",
    "Endpoint",
    "ExecResult",
    "ExecTimeout",
    "FileInfo",
    "Files",
    "OutputChunk",
    "Process",
    "Pty",
    "PtySize",
    "Sandbox",
    "SandboxNotFound",
    "SandboxState",
    "Snapshot",
    "SnapshotIncompatible",
    "VmlayerError",
]

__version__ = "0.0.0"
