"""Handles for things running inside a sandbox: ``Process`` and ``Pty``."""

from __future__ import annotations

import queue
import threading
from collections.abc import Iterator
from datetime import timedelta
from typing import TYPE_CHECKING, Any

from . import _rpc
from .errors import ExecTimeout
from .types import ExecResult, OutputChunk, PtySize
from .v1 import common_pb2, processes_pb2

if TYPE_CHECKING:
    from .client import Client


def _chunk(c: common_pb2.OutputChunk) -> OutputChunk:
    stream = "stderr" if c.stream == common_pb2.OUTPUT_STREAM_STDERR else "stdout"
    return OutputChunk(stream=stream, data=bytes(c.data))  # type: ignore[arg-type]


def _exit_code(st: common_pb2.ExitStatus) -> tuple[int, int | None]:
    if st.HasField("signal"):
        return 128 + st.signal, st.signal
    if st.HasField("code"):
        return st.code, None
    return -1, None


class _Collector:
    """Accumulates output events into an ExecResult."""

    def __init__(self) -> None:
        self.out: list[bytes] = []
        self.err: list[bytes] = []
        self.merged: list[bytes] = []
        self.dropped = 0

    def add(self, c: OutputChunk) -> None:
        (self.out if c.stream == "stdout" else self.err).append(c.data)
        self.merged.append(c.data)

    def result(self, st: common_pb2.ExitStatus) -> ExecResult:
        code, signal = _exit_code(st)
        return ExecResult(
            exit_code=code,
            stdout=b"".join(self.out).decode("utf-8", "replace"),
            stderr=b"".join(self.err).decode("utf-8", "replace"),
            output=b"".join(self.merged).decode("utf-8", "replace"),
            duration=_rpc.from_duration(st.duration),
            signal=signal,
        )


class _StdinWriter:
    """File-like writer that feeds the client-streaming Stdin RPC."""

    def __init__(self, client: Client, sandbox_id: str, pid: int) -> None:
        self._q: queue.Queue[bytes | None] = queue.Queue()
        self._client = client
        self._first = processes_pb2.StdinChunk(sandbox_id=sandbox_id, pid=pid)
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        self.closed = False

    def _gen(self) -> Iterator[processes_pb2.StdinChunk]:
        yield self._first
        while True:
            item = self._q.get()
            if item is None:
                return
            yield processes_pb2.StdinChunk(data=item)

    def _run(self) -> None:
        try:
            self._client._conn.processes.Stdin(self._gen())
        except Exception:
            pass

    def write(self, data: bytes) -> int:
        if self.closed:
            raise ValueError("stdin is closed")
        self._q.put(bytes(data))
        return len(data)

    def flush(self) -> None:
        return None

    def close(self) -> None:
        if not self.closed:
            self.closed = True
            self._q.put(None)
            self._thread.join(timeout=5)

    def __enter__(self) -> _StdinWriter:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


class Process:
    """A command started with ``Sandbox.spawn``.

    Iterate the object to stream output chunks as they arrive. Call ``wait``
    to block until exit. Use ``stdin`` to feed input.
    """

    def __init__(
        self, client: Client, sandbox_id: str, pid: int, *, timeout: timedelta | None = None
    ) -> None:
        self._client = client
        self._sandbox_id = sandbox_id
        self.pid = pid
        self._timeout = timeout
        self._stdin: _StdinWriter | None = None
        self._collector = _Collector()
        self._status: common_pb2.ExitStatus | None = None

    @property
    def stdin(self) -> _StdinWriter:
        """Writer for the process's stdin. ``close()`` it to send EOF."""
        if self._stdin is None:
            self._stdin = _StdinWriter(self._client, self._sandbox_id, self.pid)
        return self._stdin

    def _events(self) -> Iterator[processes_pb2.ExecEvent]:
        c = self._client._conn
        req = processes_pb2.OutputRequest(sandbox_id=self._sandbox_id, pid=self.pid)
        yield from c.stream(c.processes.Output, req)

    def __iter__(self) -> Iterator[OutputChunk]:
        """Yield stdout/stderr chunks until the process exits."""
        for ev in self._events():
            kind = ev.WhichOneof("event")
            if kind == "output":
                chunk = _chunk(ev.output)
                self._collector.add(chunk)
                yield chunk
            elif kind == "dropped_bytes":
                self._collector.dropped += ev.dropped_bytes
            elif kind == "exited":
                self._status = ev.exited

    def wait(self, timeout: timedelta | None = None) -> ExecResult:
        """Block until exit. Buffers any output not already consumed via iteration."""
        if self._status is None:
            for _ in self:
                pass
        if self._status is None:
            c = self._client._conn
            self._status = c.call(
                c.processes.Wait,
                processes_pb2.WaitRequest(
                    sandbox_id=self._sandbox_id, pid=self.pid, timeout=_rpc.to_duration(timeout)
                ),
                timeout=(timeout.total_seconds() + 5) if timeout else None,
            )
        result = self._collector.result(self._status)
        if self._status.timed_out and self._timeout is not None:
            raise ExecTimeout(self._timeout, result)
        return result

    def kill(self, signal: int = 9) -> None:
        c = self._client._conn
        c.call(
            c.processes.Kill,
            processes_pb2.KillProcessRequest(
                sandbox_id=self._sandbox_id, pid=self.pid, signal=signal
            ),
        )

    def __repr__(self) -> str:
        return f"Process(pid={self.pid}, sandbox={self._sandbox_id!r})"


class Pty:
    """An interactive terminal session inside the sandbox.

    Unlike ``Process``, output is a single byte stream with terminal escapes
    intact, suitable for driving a TUI or attaching to xterm.js.
    """

    def __init__(self, client: Client, open_msg: processes_pb2.PtyOpen) -> None:
        self._client = client
        self._q: queue.Queue[processes_pb2.PtyInput | None] = queue.Queue()
        self._q.put(processes_pb2.PtyInput(open=open_msg))
        self._responses = client._conn.processes.Pty(self._gen())
        self._exit: common_pb2.ExitStatus | None = None
        self.closed = False

    def _gen(self) -> Iterator[processes_pb2.PtyInput]:
        while True:
            item = self._q.get()
            if item is None:
                return
            yield item

    def __iter__(self) -> Iterator[bytes]:
        try:
            for msg in self._responses:
                kind = msg.WhichOneof("output")
                if kind == "data":
                    yield bytes(msg.data)
                elif kind == "exited":
                    self._exit = msg.exited
                    return
        except Exception as e:  # grpc errors surface here
            if not self.closed:
                raise _rpc.translate(e) from None

    def read(self, timeout: float | None = None) -> bytes:
        """Next chunk of output, or ``b""`` when the session has ended."""
        it: Any = iter(self)
        del timeout  # reserved; the gRPC stream has no per-read timeout
        try:
            return bytes(next(it))
        except StopIteration:
            return b""

    def write(self, data: bytes) -> None:
        self._q.put(processes_pb2.PtyInput(data=bytes(data)))

    def resize(self, size: PtySize) -> None:
        self._q.put(
            processes_pb2.PtyInput(resize=processes_pb2.PtySize(cols=size.cols, rows=size.rows))
        )

    def close(self) -> None:
        if not self.closed:
            self.closed = True
            self._q.put(None)
            try:
                self._responses.cancel()
            except Exception:
                pass

    def __enter__(self) -> Pty:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()
