"""Connection plumbing: channel, stubs, endpoint discovery, error translation.

Everything that touches grpc directly lives here so the public modules stay
free of wire details.
"""

from __future__ import annotations

import os
from collections.abc import Callable, Iterator, Mapping
from dataclasses import dataclass
from datetime import timedelta
from pathlib import Path
from typing import Any, TypeVar

import grpc
from google.protobuf import duration_pb2, timestamp_pb2

from . import errors
from .v1 import (
    files_pb2_grpc,
    processes_pb2_grpc,
    sandboxes_pb2_grpc,
    system_pb2_grpc,
    volumes_pb2_grpc,
)

T = TypeVar("T")


def default_endpoint() -> str:
    """Mirror the daemon's default socket location."""
    if ep := os.environ.get("VMLAYER_ENDPOINT"):
        return ep
    if sock := os.environ.get("VMLAYER_SOCKET"):
        return f"unix://{sock}"
    if runtime := os.environ.get("XDG_RUNTIME_DIR"):
        return f"unix://{Path(runtime) / 'vmlayer' / 'vmlayerd.sock'}"
    if data := os.environ.get("VMLAYER_DATA_DIR"):
        return f"unix://{Path(data) / 'vmlayerd.sock'}"
    return f"unix://{Path.home() / '.vmlayer' / 'vmlayerd.sock'}"


@dataclass(frozen=True)
class TLS:
    """TLS settings for a ``tls://`` endpoint.

    Each field is a path to a PEM file or the PEM bytes themselves.
    ``ca`` is the CA bundle that signed the daemon's certificate (default:
    the system roots); ``cert`` and ``key`` are this client's certificate
    for daemons that require mutual TLS. ``server_name`` overrides the name
    checked against the daemon's certificate.

    ``TLS.from_env()`` reads ``VMLAYER_TLS_CA``, ``VMLAYER_TLS_CERT`` and
    ``VMLAYER_TLS_KEY``.
    """

    ca: str | Path | bytes | None = None
    cert: str | Path | bytes | None = None
    key: str | Path | bytes | None = None
    server_name: str | None = None

    @classmethod
    def from_env(cls) -> TLS | None:
        ca, cert, key = (os.environ.get(f"VMLAYER_TLS_{n}") for n in ("CA", "CERT", "KEY"))
        if not (ca or cert or key):
            return None
        return cls(ca=ca, cert=cert, key=key)

    def credentials(self) -> grpc.ChannelCredentials:
        if (self.cert is None) != (self.key is None):
            raise ValueError("TLS cert and key go together")
        return grpc.ssl_channel_credentials(
            root_certificates=_pem(self.ca),
            private_key=_pem(self.key),
            certificate_chain=_pem(self.cert),
        )


def _pem(v: str | Path | bytes | None) -> bytes | None:
    if v is None or isinstance(v, bytes):
        return v
    return Path(v).read_bytes()


def grpc_target(endpoint: str) -> str:
    if endpoint.startswith("unix://"):
        return "unix:" + endpoint[len("unix://") :]
    for scheme in ("tcp://", "tls://"):
        if endpoint.startswith(scheme):
            return endpoint[len(scheme) :]
    if endpoint.startswith("/"):
        return "unix:" + endpoint
    return endpoint


class _TokenPlugin(grpc.AuthMetadataPlugin):  # type: ignore[misc]
    def __init__(self, token: str) -> None:
        self._token = token

    def __call__(self, context: Any, callback: Any) -> None:
        callback((("authorization", f"Bearer {self._token}"),), None)


class ChannelSpec:
    """Where and how to connect: shared by the sync and asyncio clients."""

    def __init__(self, endpoint: str | None, token: str | None, tls: TLS | None) -> None:
        self.endpoint = endpoint or default_endpoint()
        if token is None:
            token = os.environ.get("VMLAYER_TOKEN") or None
        if tls is None and self.endpoint.startswith("tls://"):
            tls = TLS.from_env() or TLS()
        self.target = grpc_target(self.endpoint)
        self.options: list[tuple[str, Any]] = [
            ("grpc.max_receive_message_length", 64 * 1024 * 1024),
            ("grpc.max_send_message_length", 64 * 1024 * 1024),
        ]
        if self.target.startswith("unix:"):
            # grpc-python derives :authority from the socket path, which HTTP/2
            # servers reject as malformed; pin it to something valid.
            self.options.append(("grpc.default_authority", "localhost"))
        self.credentials: grpc.ChannelCredentials | None = None
        if tls is not None:
            if tls.server_name:
                self.options.append(("grpc.ssl_target_name_override", tls.server_name))
            creds = tls.credentials()
            if token:
                creds = grpc.composite_channel_credentials(
                    creds, grpc.metadata_call_credentials(_TokenPlugin(token))
                )
            self.credentials = creds
        elif token:
            # Plaintext: grpc only sends tokens over a local connection
            # (a unix socket or loopback TCP). Remote daemons need tls://.
            self.credentials = grpc.composite_channel_credentials(
                grpc.local_channel_credentials(),
                grpc.metadata_call_credentials(_TokenPlugin(token)),
            )


class Connection:
    """One channel and its stubs."""

    def __init__(
        self,
        endpoint: str | None,
        token: str | None,
        timeout: timedelta,
        tls: TLS | None = None,
    ) -> None:
        spec = ChannelSpec(endpoint, token, tls)
        self.endpoint = spec.endpoint
        self.timeout = timeout.total_seconds()
        if spec.credentials is not None:
            self.channel = grpc.secure_channel(spec.target, spec.credentials, options=spec.options)
        else:
            self.channel = grpc.insecure_channel(spec.target, options=spec.options)
        self.sandboxes = sandboxes_pb2_grpc.SandboxesStub(self.channel)  # type: ignore[no-untyped-call]
        self.processes = processes_pb2_grpc.ProcessesStub(self.channel)  # type: ignore[no-untyped-call]
        self.files = files_pb2_grpc.FilesStub(self.channel)  # type: ignore[no-untyped-call]
        self.volumes = volumes_pb2_grpc.VolumesStub(self.channel)  # type: ignore[no-untyped-call]
        self.system = system_pb2_grpc.SystemStub(self.channel)  # type: ignore[no-untyped-call]

    def close(self) -> None:
        self.channel.close()

    def call(self, method: Callable[..., T], request: Any, *, timeout: float | None = None) -> T:
        """Unary call with error translation. ``timeout=None`` uses the default."""
        try:
            return method(request, timeout=self.timeout if timeout is None else timeout)
        except grpc.RpcError as e:
            raise translate(e) from None

    def stream(
        self, method: Callable[..., Iterator[T]], request: Any, *, timeout: float | None = None
    ) -> Iterator[T]:
        """Server-streaming call; errors surface while iterating."""
        try:
            yield from method(request, timeout=timeout)
        except grpc.RpcError as e:
            raise translate(e) from None


def translate(e: grpc.RpcError) -> errors.VmlayerError:
    code = e.code() if hasattr(e, "code") else grpc.StatusCode.UNKNOWN
    message = e.details() if hasattr(e, "details") else str(e)
    kind = ""
    try:
        for k, v in e.trailing_metadata() or ():
            if k == "vmlayer-error":
                kind = str(v)
    except Exception:  # pragma: no cover - metadata is best effort
        pass
    message = message or ""
    if code == grpc.StatusCode.UNAVAILABLE:
        return errors.DaemonUnavailable(message)
    if code == grpc.StatusCode.UNAUTHENTICATED:
        return errors.Unauthenticated(message)
    if code == grpc.StatusCode.PERMISSION_DENIED:
        return errors.PermissionDenied(message)
    if kind == "sandbox_not_found":
        return errors.SandboxNotFound(message)
    if kind == "volume_not_found":
        return errors.VolumeNotFound(message)
    if kind == "snapshot_not_found":
        return errors.SnapshotNotFound(message)
    if kind == "unsupported" or code == grpc.StatusCode.UNIMPLEMENTED:
        return errors.BackendUnsupported.from_message(message)
    if kind == "volume_busy":
        return errors.VolumeBusy(message)
    if kind == "snapshot_incompatible":
        return errors.SnapshotIncompatible(message)
    if kind == "timeout" or code == grpc.StatusCode.DEADLINE_EXCEEDED:
        return errors.TimeoutError(message)
    if code == grpc.StatusCode.RESOURCE_EXHAUSTED:
        return errors.ResourceExhausted(message)
    if code == grpc.StatusCode.INVALID_ARGUMENT:
        return errors.InvalidArgument(message)
    if code == grpc.StatusCode.NOT_FOUND:
        return errors.NotFound(message)
    return errors.VmlayerError(f"{code.name.lower()}: {message}")


# -- conversions ------------------------------------------------------------


def to_duration(d: timedelta | None) -> duration_pb2.Duration | None:
    if d is None:
        return None
    out = duration_pb2.Duration()
    out.FromTimedelta(d)
    return out


def from_timestamp(t: timestamp_pb2.Timestamp | None) -> float:
    if t is None:
        return 0.0
    return t.seconds + t.nanos / 1e9


def from_duration(d: duration_pb2.Duration | None) -> timedelta:
    if d is None:
        return timedelta(0)
    return d.ToTimedelta()


def labels_arg(labels: Mapping[str, str] | None) -> dict[str, str]:
    return dict(labels) if labels else {}
