"""The authenticated HTTPS gateway for exposed ports (decision 0021).

Starts a daemon with ``[gateway]`` on a loopback port and a wildcard
certificate for ``*.vmlayer.test``; requests reach it by IP with the
sandbox's hostname in SNI and ``Host``.
"""

from __future__ import annotations

import os
import shutil
import socket
import ssl
import subprocess
import tempfile
import time
from collections.abc import Iterator
from pathlib import Path

import pytest
from conftest import _daemon_bin
from test_auth import _free_port, _openssl

import vmlayer
from vmlayer import Sandbox

IMAGE = "python:3.12-slim"
DOMAIN = "vmlayer.test"

# Answers with the request head it received, so the test can check what
# reached the sandbox; switches to an echo stream on "Upgrade: echo".
SERVER = r"""
import socket, threading
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", 8080)); srv.listen(16)
def handle(c):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = c.recv(4096)
        if not chunk: return
        data += chunk
    head = data.split(b"\r\n\r\n")[0]
    if b"upgrade: echo" in head.lower():
        c.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: echo\r\nConnection: Upgrade\r\n\r\n")
        while True:
            b = c.recv(4096)
            if not b: break
            c.sendall(b)
    else:
        c.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(head) + head)
    c.close()
while True:
    c, _ = srv.accept(); threading.Thread(target=handle, args=(c,), daemon=True).start()
"""


class Gateway:
    def __init__(self, root: Path) -> None:
        certs = root / "certs"
        certs.mkdir()
        _openssl("req", "-x509", "-new", "-nodes", "-newkey", "rsa:2048", "-days", "2",
                 "-keyout", "ca.key", "-out", "ca.pem", "-subj", "/CN=gw-test-ca", cwd=certs)  # fmt: skip
        (certs / "gw.ext").write_text(f"subjectAltName=DNS:*.{DOMAIN}\n")
        _openssl("req", "-new", "-nodes", "-newkey", "rsa:2048", "-keyout", "gw.key",
                 "-out", "gw.csr", "-subj", f"/CN=*.{DOMAIN}", cwd=certs)  # fmt: skip
        _openssl("x509", "-req", "-in", "gw.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
                 "-CAcreateserial", "-days", "2", "-out", "gw.pem", "-extfile", "gw.ext",
                 cwd=certs)  # fmt: skip
        self.ca = certs / "ca.pem"
        self.port = _free_port()
        sock = root / "d.sock"
        (root / "vmlayerd.toml").write_text(
            f'listen = "unix://{sock}"\ndata_dir = "{root / "data"}"\n'
            f'[gateway]\nlisten = "127.0.0.1:{self.port}"\ndomain = "{DOMAIN}"\n'
            f'cert = "{certs / "gw.pem"}"\nkey = "{certs / "gw.key"}"\n'
        )
        self.proc = subprocess.Popen(
            [str(_daemon_bin()), "--config", str(root / "vmlayerd.toml")],
            stdout=open(root / "daemon.log", "wb"),
            stderr=subprocess.STDOUT,
        )
        self.client = vmlayer.Client(f"unix://{sock}")
        deadline = time.time() + 30
        while True:
            assert self.proc.poll() is None, f"daemon exited; see {root}/daemon.log"
            try:
                if self.client.health():
                    break
            except vmlayer.VmlayerError:
                assert time.time() < deadline
                time.sleep(0.2)

    def connect(self, host: str) -> ssl.SSLSocket:
        ctx = ssl.create_default_context(cafile=str(self.ca))
        raw = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        return ctx.wrap_socket(raw, server_hostname=host)

    def request(self, host: str, path: str = "/", headers: dict[str, str] | None = None) -> bytes:
        lines = [f"GET {path} HTTP/1.1", f"Host: {host}", "Connection: close"]
        lines += [f"{k}: {v}" for k, v in (headers or {}).items()]
        with self.connect(host) as s:
            s.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
            data = b""
            while chunk := s.recv(65536):
                data += chunk
        return data


@pytest.fixture(scope="module")
def gw(daemon: vmlayer.Client) -> Iterator[Gateway]:
    if shutil.which("openssl") is None:
        pytest.skip("openssl not installed")
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="gw-", dir=root))
    g = Gateway(tmp)
    try:
        yield g
    finally:
        for sb in g.client.list_sandboxes():
            sb.kill()
        g.proc.terminate()
        g.proc.wait(timeout=15)
        if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
            shutil.rmtree(tmp, ignore_errors=True)


def test_published_ports_need_their_token(gw: Gateway) -> None:
    with Sandbox.create(IMAGE, entrypoint=["python3", "-c", SERVER], client=gw.client) as sb:
        sb.wait_for_port(8080, timeout=__import__("datetime").timedelta(seconds=30))
        ep = sb.expose(8080, gateway=True)
        host = f"8080-{sb.id}.{DOMAIN}"
        assert ep.url == f"https://{host}:{gw.port}" and ep.token
        assert sb.exposed == {8080: vmlayer.Endpoint(ep.host, ep.port)}

        # No token, a wrong token, or an unpublished port: all 401.
        assert gw.request(host).startswith(b"HTTP/1.1 401")
        assert gw.request(host, headers={"X-Vmlayer-Token": "nope"}).startswith(b"HTTP/1.1 401")
        other = f"9999-{sb.id}.{DOMAIN}"
        assert gw.request(other, headers={"X-Vmlayer-Token": ep.token}).startswith(b"HTTP/1.1 401")
        assert gw.request(f"nothing.{DOMAIN}").startswith(b"HTTP/1.1 404")

        # With the token the request arrives, minus the gateway's credentials.
        r = gw.request(
            host,
            "/hello",
            {"Authorization": f"Bearer {ep.token}", "Cookie": "app=1", "X-App": "yes"},
        )
        assert r.startswith(b"HTTP/1.1 200"), r
        seen = r.split(b"\r\n\r\n", 1)[1].lower()
        assert b"get /hello" in seen and b"x-app: yes" in seen and b"cookie: app=1" in seen
        assert ep.token.encode() not in seen and b"authorization" not in seen

        # A browser: ?vmlayer_token= once sets an HTTP-only cookie and redirects.
        r = gw.request(host, f"/page?a=1&vmlayer_token={ep.token}")
        assert r.startswith(b"HTTP/1.1 302"), r
        head = r.split(b"\r\n\r\n")[0].decode().lower()
        assert "location: /page?a=1" in head and "httponly" in head and "secure" in head
        r = gw.request(host, "/page?a=1", {"Cookie": f"vmlayer_token={ep.token}"})
        assert r.startswith(b"HTTP/1.1 200")

        # Upgrades (WebSocket and friends) pass through as a byte stream.
        with gw.connect(host) as s:
            s.sendall(
                f"GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n"
                f"X-Vmlayer-Token: {ep.token}\r\n\r\n".encode()
            )
            head = b""
            while b"\r\n\r\n" not in head:
                head += s.recv(4096)
            assert head.startswith(b"HTTP/1.1 101"), head
            s.sendall(b"ping-over-the-gateway")
            echoed = b""
            while len(echoed) < len(b"ping-over-the-gateway"):
                echoed += s.recv(4096)
            assert echoed == b"ping-over-the-gateway"

        # Unexpose withdraws the route.
        sb.unexpose(8080)
        assert gw.request(host, headers={"X-Vmlayer-Token": ep.token}).startswith(b"HTTP/1.1 401")


def test_gateway_off_is_refused(daemon: vmlayer.Client) -> None:
    with Sandbox.create("alpine:3.20", client=daemon) as sb, pytest.raises(vmlayer.VmlayerError, match="gateway is off"):
        sb.expose(8080, gateway=True)
