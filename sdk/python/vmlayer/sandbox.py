"""``Sandbox``: the unit of isolation and the entry point of the SDK."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from datetime import timedelta
from typing import TYPE_CHECKING, Literal

from . import _rpc
from .client import Client, _capabilities
from .errors import ExecTimeout
from .files import Files
from .process import Process, Pty, _chunk, _Collector
from .types import (
    AUTO,
    DEFAULT_PTY_SIZE,
    Backend,
    Capabilities,
    Endpoint,
    ExecResult,
    IoLimits,
    ProcessInfo,
    PtySize,
    SandboxState,
)
from .v1 import common_pb2, processes_pb2, sandboxes_pb2
from .volume import Snapshot, Volume, _client

if TYPE_CHECKING:
    pass


_STATES = {
    common_pb2.SANDBOX_STATE_CREATING: SandboxState.CREATING,
    common_pb2.SANDBOX_STATE_RUNNING: SandboxState.RUNNING,
    common_pb2.SANDBOX_STATE_PAUSED: SandboxState.PAUSED,
    common_pb2.SANDBOX_STATE_STOPPED: SandboxState.STOPPED,
    common_pb2.SANDBOX_STATE_FAILED: SandboxState.FAILED,
}


def _argv(cmd: str | Sequence[str]) -> list[str]:
    if isinstance(cmd, str):
        return ["/bin/sh", "-c", cmd]
    argv = list(cmd)
    if not argv:
        raise ValueError("command must not be empty")
    return argv


def _create_request(
    image: str,
    *,
    cpus: int | None,
    memory_mb: int | None,
    disk_mb: int | None,
    env: Mapping[str, str] | None,
    workdir: str | None,
    workspace: Volume | str | type[AUTO] | None,
    mounts: Mapping[str, Volume | tuple[Volume, Literal["ro", "rw"]]] | None,
    entrypoint: Sequence[str] | type[AUTO] | None,
    timeout: timedelta | None,
    backend: Backend,
    network: bool,
    egress: Sequence[str] | None,
    limits: IoLimits | None,
    lease: str | None,
    reschedule: bool,
    labels: Mapping[str, str] | None,
    credentials: Sequence[str] = (),
) -> sandboxes_pb2.CreateSandboxRequest:
    """The Create request for ``Sandbox.create``; shared with ``vmlayer.aio``."""
    req = sandboxes_pb2.CreateSandboxRequest(
        image=image,
        vcpus=cpus or 0,
        memory_mib=memory_mb or 0,
        env=dict(env or {}),
        workdir=workdir or "",
        backend=backend.value,
        network=common_pb2.NETWORK_MODE_EGRESS_AND_EXPOSE
        if network
        else common_pb2.NETWORK_MODE_NONE,
        labels=_rpc.labels_arg(labels),
        ttl=_rpc.to_duration(timeout),
    )
    if disk_mb is not None:
        req.disk_mib = disk_mb
    if lease:
        req.lease = lease
    req.reschedule = reschedule
    if isinstance(credentials, str):
        raise TypeError("credentials takes a list of names, not a string")
    req.credentials.extend(credentials)
    if egress is not None:
        if isinstance(egress, str):
            raise TypeError("egress takes a list of rules, not a string")
        req.egress.allow.extend(egress)
        req.egress.SetInParent()
    if limits is not None:
        for name in ("network_mbps", "disk_mbps", "disk_iops"):
            value = getattr(limits, name)
            if value is not None:
                setattr(req.limits, name, value)
        req.limits.SetInParent()
    if workspace is AUTO:
        req.workspace.anonymous.SetInParent()
    elif workspace is None:
        req.workspace.none.SetInParent()
    else:
        req.workspace.volume = workspace if isinstance(workspace, str) else workspace.id  # type: ignore[union-attr]
    for path, spec in (mounts or {}).items():
        vol, mode = spec if isinstance(spec, tuple) else (spec, "rw")
        req.mounts.append(
            common_pb2.Mount(volume_id=vol.id, guest_path=path, read_only=(mode == "ro"))
        )
    if entrypoint is AUTO:
        req.entrypoint.image_default.SetInParent()
    elif entrypoint is None:
        req.entrypoint.none.SetInParent()
    else:
        req.entrypoint.argv.argv.extend(list(entrypoint))  # type: ignore[arg-type]
    return req


def _exec_request(
    sandbox_id: str,
    cmd: str | Sequence[str],
    *,
    cwd: str | None,
    env: Mapping[str, str] | None,
    timeout: timedelta | None,
    stdin: bytes | None = None,
    detach: bool = False,
    user: str | None = None,
) -> processes_pb2.ExecRequest:
    return processes_pb2.ExecRequest(
        user=user or "",
        sandbox_id=sandbox_id,
        argv=_argv(cmd),
        cwd=cwd or "",
        env=dict(env or {}),
        timeout=_rpc.to_duration(timeout),
        stdin=stdin or b"",
        detach=detach,
    )


class _ExecFold:
    """Folds the events of an Exec stream into an ``ExecResult``."""

    def __init__(self) -> None:
        self._collector = _Collector()
        self._status: common_pb2.ExitStatus | None = None

    def feed(self, ev: processes_pb2.ExecEvent) -> None:
        kind = ev.WhichOneof("event")
        if kind == "output":
            self._collector.add(_chunk(ev.output))
        elif kind == "exited":
            self._status = ev.exited

    def result(self, timeout: timedelta | None) -> ExecResult:
        if self._status is None:
            raise RuntimeError("exec stream ended without an exit status")
        result = self._collector.result(self._status)
        if self._status.timed_out and timeout is not None:
            raise ExecTimeout(timeout, result)
        return result


def _endpoint(e: common_pb2.Endpoint) -> Endpoint:
    return Endpoint(e.host, e.port, gateway_url=e.url or None, token=e.token or None)


def _process_info(p: common_pb2.ProcessInfo) -> ProcessInfo:
    return ProcessInfo(
        pid=p.pid,
        argv=tuple(p.argv),
        started_at=_rpc.from_timestamp(p.started_at),
        is_entrypoint=p.is_entrypoint,
    )


class _Fields:
    """Attributes read from the daemon's view of a sandbox; shared with
    ``vmlayer.aio.AsyncSandbox``."""

    _pb: common_pb2.Sandbox

    @property
    def id(self) -> str:
        return self._pb.id

    @property
    def backend(self) -> Backend:
        try:
            return Backend(self._pb.backend)
        except ValueError:
            return Backend.AUTO

    @property
    def capabilities(self) -> Capabilities:
        return _capabilities(self._pb.capabilities)

    @property
    def image(self) -> str:
        return self._pb.image

    @property
    def workdir(self) -> str:
        return self._pb.workdir

    @property
    def labels(self) -> Mapping[str, str]:
        return dict(self._pb.labels)

    @property
    def created_at(self) -> float:
        """Unix timestamp, seconds."""
        return _rpc.from_timestamp(self._pb.created_at)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, _Fields) and other.id == self.id

    def __hash__(self) -> int:
        return hash(self.id)


class Sandbox(_Fields):
    """One isolated environment. Create with ``Sandbox.create``, restore with
    ``Sandbox.restore``, attach to an existing one with ``Sandbox.connect``.

    Works as a context manager; leaving the block calls ``kill``.
    """

    def __init__(self, client: Client, pb: common_pb2.Sandbox) -> None:
        self._client = client
        self._pb = pb
        self.files = Files(client, pb.id)

    @classmethod
    def _from_pb(cls, client: Client, pb: common_pb2.Sandbox) -> Sandbox:
        return cls(client, pb)

    # -- attributes ---------------------------------------------------------

    @property
    def workspace(self) -> Volume | None:
        """The volume mounted at ``workdir``, or ``None`` if created with
        ``workspace=None``."""
        if not self._pb.workspace_volume_id:
            return None
        return Volume.get(self._pb.workspace_volume_id, client=self._client)

    @property
    def mounts(self) -> Mapping[str, Volume]:
        """Every attached volume by guest path, ``workspace`` included."""
        return {m.guest_path: Volume.get(m.volume_id, client=self._client) for m in self._pb.mounts}

    @property
    def state(self) -> SandboxState:
        self.refresh()
        return _STATES.get(self._pb.state, SandboxState.FAILED)

    @property
    def exposed(self) -> Mapping[int, Endpoint]:
        """Guest ports currently exposed, by guest port."""
        self.refresh()
        return {int(p): Endpoint(e.host, e.port) for p, e in self._pb.exposed.items()}

    def refresh(self) -> Sandbox:
        c = self._client._conn
        self._pb = c.call(c.sandboxes.Get, sandboxes_pb2.GetSandboxRequest(id=self.id))
        return self

    def __repr__(self) -> str:
        return f"Sandbox(id={self.id!r}, image={self.image!r}, backend={self._pb.backend!r})"

    # -- construction -------------------------------------------------------

    @classmethod
    def create(
        cls,
        image: str,
        *,
        cpus: int | None = None,
        memory_mb: int | None = None,
        disk_mb: int | None = None,
        env: Mapping[str, str] | None = None,
        workdir: str | None = None,
        workspace: Volume | str | type[AUTO] | None = AUTO,
        mounts: Mapping[str, Volume | tuple[Volume, Literal["ro", "rw"]]] | None = None,
        entrypoint: Sequence[str] | type[AUTO] | None = AUTO,
        timeout: timedelta | None = timedelta(minutes=15),
        backend: Backend = Backend.AUTO,
        network: bool = True,
        egress: Sequence[str] | None = None,
        limits: IoLimits | None = None,
        lease: str | None = None,
        reschedule: bool = False,
        labels: Mapping[str, str] | None = None,
        credentials: Sequence[str] = (),
        client: Client | None = None,
    ) -> Sandbox:
        """Boot a new sandbox from an image.

        ``image`` is an OCI reference (``python:3.12``,
        ``ghcr.io/org/agent-base:v3``). The daemon converts OCI images to the
        backend's rootfs format and caches the result. The root disk is
        disposable: it is the image plus a thin overlay.

        ``workspace`` is the volume mounted at ``workdir``:

        * ``AUTO`` (default): a fresh anonymous volume that is deleted with
          the sandbox. Most callers never touch this.
        * a ``Volume`` or its name/id: reuse persistent storage. Fails if it is
          already attached read-write elsewhere.
        * ``None``: no workspace volume; work on the root overlay only.
          Slightly faster boot, nothing survives the sandbox.

        ``mounts`` attaches additional volumes at other paths, read-write by
        default; use ``(volume, "ro")`` for shared read-only data.

        ``entrypoint`` is a command the guest agent starts as soon as the
        sandbox is up, for images that *are* a server (Jupyter kernel gateway,
        an agent server, a dev server). ``AUTO`` uses the image's
        ``ENTRYPOINT``/``CMD``; ``None`` starts nothing. Its output is
        available via ``process(1)``. Combine with ``wait_for_port`` and
        ``expose``.

        ``timeout`` is a hard wall-clock lifetime; the sandbox is killed when
        it elapses. ``None`` disables it.

        ``egress`` is an allowlist of where the sandbox may connect, e.g.
        ``["pypi.org", "*.pythonhosted.org:443", "10.0.0.0/8:5432"]``. Anything
        else is rejected, and DNS answers only allowed names. ``[]`` blocks all
        egress. ``None`` (default) allows everything except cloud metadata
        endpoints, which are always blocked.

        ``limits`` caps network and disk throughput (``IoLimits``).

        ``egress`` and ``limits`` need a microVM backend; a backend that cannot
        enforce them raises ``BackendUnsupported`` (see ``capabilities``).

        ``credentials`` names API credentials the daemon's operator configured
        (``[credential_proxy]``). The sandbox gets placeholders and base URLs
        for them in its environment; the daemon swaps in the real secrets on
        the way out, so code in the sandbox never sees them.

        ``lease`` names the sandbox cluster-wide: while it lives, creating
        another sandbox with the same lease on any daemon sharing the blob
        store fails. A daemon that loses the lease (cut off from the store
        past the lease TTL) kills its sandbox, so two never run at once.

        ``reschedule=True`` (with ``lease``) checkpoints the workspace to the
        blob store periodically; if this host dies, a daemon running with
        ``--rescue`` recreates the sandbox from the last checkpoint. It boots
        fresh: memory state and writes after the last checkpoint are lost.
        """
        c = _client(client)
        req = _create_request(
            image,
            cpus=cpus,
            memory_mb=memory_mb,
            disk_mb=disk_mb,
            env=env,
            workdir=workdir,
            workspace=workspace,
            mounts=mounts,
            entrypoint=entrypoint,
            timeout=timeout,
            backend=backend,
            network=network,
            egress=egress,
            limits=limits,
            lease=lease,
            reschedule=reschedule,
            labels=labels,
            credentials=credentials,
        )
        pb = c._conn.call(c._conn.sandboxes.Create, req, timeout=600)
        return cls(c, pb)

    @classmethod
    def restore(
        cls,
        snapshot: Snapshot | str,
        *,
        timeout: timedelta | None = timedelta(minutes=15),
        labels: Mapping[str, str] | None = None,
        client: Client | None = None,
    ) -> Sandbox:
        """Start a new sandbox from a snapshot.

        With a memory snapshot this resumes exactly where ``snapshot()`` was
        called, including running processes. Every volume that was attached
        comes back as a fresh copy-on-write clone; the originals are not
        touched. Raises ``SnapshotIncompatible`` if the host cannot run it.
        """
        c = _client(client)
        sid = snapshot if isinstance(snapshot, str) else snapshot.id
        req = sandboxes_pb2.RestoreSandboxRequest(
            snapshot_id=sid, ttl=_rpc.to_duration(timeout), labels=_rpc.labels_arg(labels)
        )
        return cls(c, c._conn.call(c._conn.sandboxes.Restore, req, timeout=600))

    @classmethod
    def connect(cls, sandbox_id: str, *, client: Client | None = None) -> Sandbox:
        """Attach to a sandbox that already exists on the daemon."""
        c = _client(client)
        return cls(
            c, c._conn.call(c._conn.sandboxes.Get, sandboxes_pb2.GetSandboxRequest(id=sandbox_id))
        )

    # -- commands -------------------------------------------------------------

    def exec(
        self,
        cmd: str | Sequence[str],
        *,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        stdin: bytes | None = None,
        timeout: timedelta | None = timedelta(minutes=5),
        user: str | None = None,
    ) -> ExecResult:
        """Run a command to completion and return its output.

        A ``str`` is run through ``/bin/sh -c``; a sequence is exec'd directly.
        ``user`` runs it as a name from the guest's ``/etc/passwd``, a UID, or
        ``"uid:gid"`` (default: the image's user, usually root).
        Raises ``ExecTimeout`` if ``timeout`` elapses. A non-zero exit code is
        *not* an exception; check ``result.ok``.
        """
        c = self._client._conn
        req = _exec_request(self.id, cmd, cwd=cwd, env=env, timeout=timeout, stdin=stdin, user=user)
        fold = _ExecFold()
        rpc_timeout = (timeout.total_seconds() + 30) if timeout else None
        for ev in c.stream(c.processes.Exec, req, timeout=rpc_timeout):
            fold.feed(ev)
        return fold.result(timeout)

    def spawn(
        self,
        cmd: str | Sequence[str],
        *,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        timeout: timedelta | None = None,
        user: str | None = None,
    ) -> Process:
        """Start a command and return immediately with a streaming handle."""
        c = self._client._conn
        req = _exec_request(self.id, cmd, cwd=cwd, env=env, timeout=timeout, detach=True, user=user)
        pid = 0
        for ev in c.stream(c.processes.Exec, req, timeout=c.timeout):
            if ev.WhichOneof("event") == "started":
                pid = ev.started.pid
                break
        return Process(self._client, self.id, pid, timeout=timeout)

    def processes(self) -> list[ProcessInfo]:
        """Every process started through vmlayer that is still running,
        including the entrypoint."""
        c = self._client._conn
        r = c.call(c.processes.List, processes_pb2.ListProcessesRequest(sandbox_id=self.id))
        return [_process_info(p) for p in r.processes]

    def process(self, pid: int) -> Process:
        """Re-attach to a running process by pid. ``process(1)`` is the entrypoint."""
        return Process(self._client, self.id, pid)

    def pty(
        self,
        cmd: str | Sequence[str] = "/bin/bash",
        *,
        size: PtySize = DEFAULT_PTY_SIZE,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        user: str | None = None,
    ) -> Pty:
        """Open an interactive terminal. Requires ``capabilities.pty``."""
        argv = [cmd] if isinstance(cmd, str) else list(cmd)
        open_msg = processes_pb2.PtyOpen(
            sandbox_id=self.id,
            argv=argv,
            cwd=cwd or "",
            env=dict(env or {}),
            size=processes_pb2.PtySize(cols=size.cols, rows=size.rows),
            user=user or "",
        )
        return Pty(self._client, open_msg)

    # -- state ----------------------------------------------------------------

    def snapshot(
        self,
        *,
        labels: Mapping[str, str] | None = None,
        stay_paused: bool = False,
        durable: bool = False,
    ) -> Snapshot:
        """Freeze the sandbox and persist it.

        On microVM backends this captures RAM, the root overlay and every
        attached volume atomically; the sandbox is paused for the duration
        (tens of ms) and then resumed. On Docker only filesystems are
        captured. Requires ``capabilities.snapshot``.

        For a cheap, portable, long-lived checkpoint of just the files, use
        ``sandbox.workspace.snapshot()`` instead.

        With ``durable=True`` the snapshot (memory, VM state and every volume)
        is also stored in the daemon's blob store, and ``Sandbox.restore(id)``
        on any daemon sharing the store resumes it, given the same CPU model
        and engine release. Microvm backends only.
        """
        c = self._client._conn
        req = sandboxes_pb2.SnapshotSandboxRequest(
            id=self.id, labels=_rpc.labels_arg(labels), stay_paused=stay_paused, durable=durable
        )
        return Snapshot._from_pb(self._client, c.call(c.sandboxes.TakeSnapshot, req, timeout=3600))

    def fork(self, count: int = 1) -> list[Sandbox]:
        """Clone this sandbox ``count`` times, each copy continuing from the
        current state with its own identity (new id, new network address) and
        its own copy-on-write clones of every attached volume.

        The parent keeps running. Requires ``capabilities.fork``.
        """
        c = self._client._conn
        r = c.call(
            c.sandboxes.Fork, sandboxes_pb2.ForkSandboxRequest(id=self.id, count=count), timeout=600
        )
        return [Sandbox(self._client, child) for child in r.children]

    def pause(self) -> None:
        """Stop the clock without freeing resources. Requires ``capabilities.pause``."""
        c = self._client._conn
        c.call(c.sandboxes.Pause, sandboxes_pb2.PauseSandboxRequest(id=self.id))

    def resume(self) -> None:
        c = self._client._conn
        c.call(c.sandboxes.Resume, sandboxes_pb2.ResumeSandboxRequest(id=self.id))

    def kill(self) -> None:
        """Destroy the sandbox and release everything. Idempotent."""
        c = self._client._conn
        c.call(c.sandboxes.Kill, sandboxes_pb2.KillSandboxRequest(id=self.id), timeout=120)

    # -- volumes --------------------------------------------------------------

    def attach(self, volume: Volume, path: str, *, read_only: bool = False) -> None:
        """Mount a volume while running. Requires ``capabilities.hot_attach``,
        which no v1 backend has; attach at ``create`` time instead."""
        c = self._client._conn
        m = common_pb2.Mount(volume_id=volume.id, guest_path=path, read_only=read_only)
        c.call(c.sandboxes.Attach, sandboxes_pb2.AttachRequest(id=self.id, mount=m))

    def detach(self, path: str) -> None:
        """Requires ``capabilities.hot_attach``."""
        c = self._client._conn
        c.call(c.sandboxes.Detach, sandboxes_pb2.DetachRequest(id=self.id, guest_path=path))

    # -- network --------------------------------------------------------------

    def expose(self, port: int, *, gateway: bool = False) -> Endpoint:
        """Make a guest port reachable from the host. Requires ``capabilities.expose_port``.

        ``gateway=True`` also publishes it on the daemon's authenticated
        HTTPS gateway: the result's ``url`` is then
        ``https://<port>-<sandbox-id>.<domain>`` and ``token`` the credential
        to send. Needs ``[gateway]`` in the daemon config."""
        c = self._client._conn
        e = c.call(
            c.sandboxes.Expose,
            sandboxes_pb2.ExposeRequest(id=self.id, guest_port=port, gateway=gateway),
            timeout=120,
        )
        return _endpoint(e)

    def unexpose(self, port: int) -> None:
        c = self._client._conn
        c.call(c.sandboxes.Unexpose, sandboxes_pb2.UnexposeRequest(id=self.id, guest_port=port))

    def wait_for_port(self, port: int, *, timeout: timedelta = timedelta(seconds=30)) -> None:
        """Block until something inside the guest listens on ``port``.

        The usual sequence for server-shaped images::

            sb = Sandbox.create("jupyter/kernel-gateway")
            sb.wait_for_port(8888)
            url = sb.expose(8888).url
        """
        c = self._client._conn
        req = sandboxes_pb2.WaitForPortRequest(
            id=self.id, port=port, timeout=_rpc.to_duration(timeout)
        )
        c.call(c.sandboxes.WaitForPort, req, timeout=timeout.total_seconds() + 10)

    # -- context manager ------------------------------------------------------

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *exc: object) -> None:
        self.kill()
