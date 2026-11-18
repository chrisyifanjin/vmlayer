"""vmlayer: the layer between agent runtimes and microVMs.

Step 0 of the project: this package is the API *surface* only. Every method
raises ``NotImplementedError``. The point is to agree on the shape before any
backend exists.

Quick tour::

    from vmlayer import Sandbox, Volume

    with Sandbox.create("python:3.12", memory_mb=512) as sb:
        result = sb.exec("python -c 'print(40 + 2)'")
        print(result.stdout)                 # "42\\n"

        sb.files.write("/work/main.py", b"print('hi')")
        proc = sb.spawn("python /work/main.py")
        for chunk in proc:
            print(chunk.data.decode(), end="")

        snap = sb.snapshot()                 # freeze memory + disk
        children = sb.fork(4)                # four copies, same state
        ckpt = sb.workspace.snapshot()       # files only, cheap, portable

    sb2 = Sandbox.restore(snap)              # ms-scale cold start

    # a workspace that outlives sandboxes
    repo = Volume.create(name="proj-42", size_mb=8192)
    with Sandbox.create("python:3.12", workspace=repo) as sb:
        sb.exec("git clone https://example.com/repo .")
    with Sandbox.create("python:3.12", workspace=repo) as sb:
        sb.exec("pytest")                    # same files, new sandbox
"""

from .client import Client, default_client, set_default_client
from .cluster import Cluster, NoCapacity
from .errors import (
    BackendUnsupported,
    DaemonUnavailable,
    ExecTimeout,
    InvalidArgument,
    NotFound,
    ResourceExhausted,
    SandboxNotFound,
    SnapshotIncompatible,
    SnapshotNotFound,
    TimeoutError,
    VmlayerError,
    VolumeBusy,
    VolumeNotFound,
)
from .files import Files
from .process import Process, Pty
from .sandbox import Sandbox
from .types import (
    AUTO,
    Backend,
    BackendInfo,
    Capabilities,
    Endpoint,
    ExecResult,
    FileInfo,
    GarbageCollection,
    HostInfo,
    ImageInfo,
    IoLimits,
    OutputChunk,
    ProcessInfo,
    PtySize,
    SandboxState,
    SystemInfo,
)
from .volume import Snapshot, Volume, VolumeSnapshot

__all__ = [
    "AUTO",
    "Backend",
    "BackendInfo",
    "BackendUnsupported",
    "Capabilities",
    "Client",
    "Cluster",
    "DaemonUnavailable",
    "Endpoint",
    "ExecResult",
    "ExecTimeout",
    "FileInfo",
    "Files",
    "GarbageCollection",
    "HostInfo",
    "ImageInfo",
    "InvalidArgument",
    "IoLimits",
    "NoCapacity",
    "NotFound",
    "OutputChunk",
    "Process",
    "ProcessInfo",
    "Pty",
    "PtySize",
    "ResourceExhausted",
    "Sandbox",
    "SandboxNotFound",
    "SandboxState",
    "Snapshot",
    "SnapshotIncompatible",
    "SnapshotNotFound",
    "SystemInfo",
    "TimeoutError",
    "VmlayerError",
    "Volume",
    "VolumeBusy",
    "VolumeNotFound",
    "VolumeSnapshot",
    "default_client",
    "set_default_client",
]

__version__ = "0.1.0"
