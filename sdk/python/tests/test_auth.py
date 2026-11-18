"""A daemon on TCP with mutual TLS and tenant-scoped tokens (decision 0020).

Starts its own daemon on a loopback port with a throwaway CA, a server
certificate, a client certificate and four tokens: two tenants, a read-only
token for one of them, and an administrator.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import socket
import subprocess
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from conftest import _daemon_bin

import vmlayer
from vmlayer import Sandbox, Volume

ALPINE = "alpine:3.20"
TOKENS = {
    "acme": ("acme", "write"),
    "globex": ("globex", "write"),
    "acme-reader": ("acme", "read"),
    "ops": (None, "admin"),
}


def _openssl(*args: str, cwd: Path) -> None:
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


def _make_certs(d: Path) -> None:
    """A CA, a server certificate for localhost and a client certificate."""
    _openssl("req", "-x509", "-new", "-nodes", "-newkey", "rsa:2048", "-days", "2",
             "-keyout", "ca.key", "-out", "ca.pem", "-subj", "/CN=vmlayer-test-ca",
             cwd=d)  # fmt: skip
    for name, ext in (
        ("server", "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n"),
        ("client", "extendedKeyUsage=clientAuth\n"),
    ):
        (d / f"{name}.ext").write_text(ext)
        _openssl("req", "-new", "-nodes", "-newkey", "rsa:2048", "-keyout", f"{name}.key",
                 "-out", f"{name}.csr", "-subj", f"/CN={name}", cwd=d)  # fmt: skip
        _openssl("x509", "-req", "-in", f"{name}.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
                 "-CAcreateserial", "-days", "2", "-out", f"{name}.pem",
                 "-extfile", f"{name}.ext", cwd=d)  # fmt: skip


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class Secured:
    def __init__(self, root: Path) -> None:
        self.dir = root
        self.certs = root / "certs"
        self.certs.mkdir()
        _make_certs(self.certs)
        self.port = _free_port()
        self.endpoint = f"tls://localhost:{self.port}"
        self.secrets = {name: f"secret-{name}-{os.urandom(8).hex()}" for name in TOKENS}
        lines = [
            f'listen = "tcp://127.0.0.1:{self.port}"',
            f'data_dir = "{root / "data"}"',
            "[tls]",
            f'cert = "{self.certs / "server.pem"}"',
            f'key = "{self.certs / "server.key"}"',
            f'client_ca = "{self.certs / "ca.pem"}"',
        ]
        for name, (tenant, role) in TOKENS.items():
            digest = hashlib.sha256(self.secrets[name].encode()).hexdigest()
            lines += ["[[auth.tokens]]", f'name = "{name}"', f'sha256 = "{digest}"']
            lines += [f'role = "{role}"'] + ([f'tenant = "{tenant}"'] if tenant else [])
        (root / "vmlayerd.toml").write_text("\n".join(lines) + "\n")
        self.proc = subprocess.Popen(
            [str(_daemon_bin()), "--config", str(root / "vmlayerd.toml")],
            stdout=open(root / "daemon.log", "wb"),
            stderr=subprocess.STDOUT,
        )
        self.tls = vmlayer.TLS(
            ca=self.certs / "ca.pem", cert=self.certs / "client.pem", key=self.certs / "client.key"
        )
        ops = self.client("ops")
        deadline = time.time() + 30
        while True:
            assert self.proc.poll() is None, f"daemon exited; see {root}/daemon.log"
            try:
                if ops.health():
                    break
            except vmlayer.VmlayerError:
                assert time.time() < deadline, f"daemon not healthy; see {root}/daemon.log"
                time.sleep(0.2)

    def client(self, name: str, tls: vmlayer.TLS | None = None) -> vmlayer.Client:
        return vmlayer.Client(self.endpoint, token=self.secrets[name], tls=tls or self.tls)

    def stop(self) -> None:
        ops = self.client("ops")
        for sb in ops.list_sandboxes():
            sb.kill()
        for v in ops.list_volumes():
            if not v.anonymous:
                v.delete()
        self.proc.terminate()
        self.proc.wait(timeout=15)


@pytest.fixture(scope="module")
def secured(daemon: vmlayer.Client) -> Iterator[Secured]:
    if shutil.which("openssl") is None:
        pytest.skip("openssl not installed")
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="auth-", dir=root))
    s = Secured(tmp)
    try:
        yield s
    finally:
        if s.proc.poll() is None:
            s.stop()
        if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
            shutil.rmtree(tmp, ignore_errors=True)


def test_transport_needs_tls_a_client_certificate_and_a_token(secured: Secured) -> None:
    # Plaintext to a TLS port.
    with pytest.raises(vmlayer.DaemonUnavailable):
        vmlayer.Client(f"tcp://127.0.0.1:{secured.port}", token=secured.secrets["ops"]).health()
    # TLS without a client certificate: the handshake fails.
    no_cert = vmlayer.TLS(ca=secured.certs / "ca.pem")
    with pytest.raises(vmlayer.DaemonUnavailable):
        secured.client("ops", tls=no_cert).health()
    # A client certificate but a wrong token.
    with pytest.raises(vmlayer.Unauthenticated):
        vmlayer.Client(secured.endpoint, token="nope", tls=secured.tls).health()
    assert secured.client("ops").health()


def test_tenants_see_only_their_own_resources(secured: Secured) -> None:
    acme, globex, ops = secured.client("acme"), secured.client("globex"), secured.client("ops")
    ws = Volume.create(name="ws", client=acme)
    sb = Sandbox.create(ALPINE, workspace="ws", client=acme)
    try:
        sb.exec("sh -c 'echo acme > /work/owner'")
        assert ws.name == "ws", "a tenant sees its own volume names"

        # Another tenant: not listed, not reachable, same error as a missing ID.
        assert sb.id not in {s.id for s in globex.list_sandboxes()}
        assert "ws" not in {v.name for v in globex.list_volumes()}
        with pytest.raises(vmlayer.SandboxNotFound):
            Sandbox.connect(sb.id, client=globex)
        with pytest.raises(vmlayer.SandboxNotFound):
            Sandbox.connect("sb-does-not-exist", client=globex)
        with pytest.raises(vmlayer.VolumeNotFound):
            Volume.get(ws.id, client=globex)
        with pytest.raises(vmlayer.VolumeNotFound):
            Sandbox.create(ALPINE, workspace=ws.id, client=globex)

        # Volume names are per tenant: globex gets its own "ws".
        theirs = Volume.create(name="ws", client=globex)
        assert theirs.id != ws.id and theirs.name == "ws"

        # Snapshots and forks keep the tenant.
        snap = sb.snapshot()
        assert snap.id not in {s.id for s in globex.list_snapshots()}
        restored = Sandbox.restore(snap.id, client=acme)
        try:
            assert restored.exec("cat /work/owner").stdout.strip() == "acme"
            assert restored.id not in {s.id for s in globex.list_sandboxes()}
        finally:
            restored.kill()
        with pytest.raises(vmlayer.SnapshotNotFound):
            Sandbox.restore(snap.id, client=globex)

        # The unscoped administrator sees both tenants.
        ids = {s.id for s in ops.list_sandboxes()}
        assert sb.id in ids
        names = {v.name for v in ops.list_volumes()}
        assert {"acme/ws", "globex/ws"} <= names
    finally:
        sb.kill()


def test_roles_limit_what_a_token_may_call(secured: Secured) -> None:
    acme, reader = secured.client("acme"), secured.client("acme-reader")
    sb = Sandbox.create(ALPINE, client=acme)
    try:
        # Read: list and inspect, nothing that changes state.
        assert sb.id in {s.id for s in reader.list_sandboxes()}
        with pytest.raises(vmlayer.PermissionDenied):
            Sandbox.create(ALPINE, client=reader)
        with pytest.raises(vmlayer.PermissionDenied):
            Sandbox.connect(sb.id, client=reader).exec("true")
        # Host-wide operations need the admin role.
        with pytest.raises(vmlayer.PermissionDenied):
            acme.collect_garbage(dry_run=True)
    finally:
        sb.kill()
    # A tenant's view of the host omits the operator's layout.
    assert acme.info().data_dir == ""
    assert secured.client("ops").info().data_dir != ""
