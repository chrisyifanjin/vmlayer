"""The credential proxy on the egress path (decision 0022).

A fake upstream on this machine echoes what it receives. The daemon holds
the secret; the sandbox gets a placeholder and a base URL at vmlayer.host,
and the upstream sees the real secret.
"""

from __future__ import annotations

import http.server
import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Iterator
from pathlib import Path

import pytest
from conftest import _daemon_bin
from test_auth import _free_port

import vmlayer
from vmlayer import Sandbox

IMAGE = "python:3.12-slim"
SECRET = "sk-real-secret-" + os.urandom(6).hex()

# Inside the sandbox: call the API the way an SDK would, from env vars only.
CALL = r"""
import json, os, urllib.request, urllib.error
req = urllib.request.Request(
    os.environ["ECHO_BASE_URL"] + "/v1/models?limit=2",
    data=b'{"q": 1}',
    headers={"Authorization": "Bearer " + os.environ["ECHO_API_KEY"], "X-App": "yes"},
)
try:
    print(urllib.request.urlopen(req, timeout=10).read().decode())
except urllib.error.HTTPError as e:
    print(json.dumps({"status": e.code}))
"""


class Echo(http.server.BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        out = json.dumps(
            {"path": self.path, "headers": dict(self.headers), "body": body.decode()}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *args: object) -> None:
        pass


class Proxied:
    def __init__(self, root: Path) -> None:
        self.upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Echo)
        threading.Thread(target=self.upstream.serve_forever, daemon=True).start()
        self.port = _free_port()
        sock = root / "d.sock"
        (root / "vmlayerd.toml").write_text(
            f'listen = "unix://{sock}"\ndata_dir = "{root / "data"}"\n'
            f'[credential_proxy]\nlisten = "0.0.0.0:{self.port}"\n'
            "[[credential_proxy.credentials]]\n"
            'name = "echo"\n'
            f'upstream = "http://127.0.0.1:{self.upstream.server_port}"\n'
            'header = "authorization"\nvalue = "Bearer {secret}"\n'
            'secret_env = "VMLAYER_TEST_ECHO_SECRET"\n'
            'guest_env = { ECHO_BASE_URL = "{url}", ECHO_API_KEY = "{placeholder}" }\n'
        )
        self.proc = subprocess.Popen(
            [str(_daemon_bin()), "--config", str(root / "vmlayerd.toml")],
            stdout=open(root / "daemon.log", "wb"),
            stderr=subprocess.STDOUT,
            env=dict(os.environ, VMLAYER_TEST_ECHO_SECRET=SECRET),
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

    def from_host(self, placeholder: str) -> int:
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}/echo/v1/x",
            data=b"{}",
            headers={"Authorization": f"Bearer {placeholder}"},
        )
        try:
            return int(urllib.request.urlopen(req, timeout=10).status)
        except urllib.error.HTTPError as e:
            return int(e.code)


@pytest.fixture(scope="module")
def proxied(daemon: vmlayer.Client) -> Iterator[Proxied]:
    root = Path(os.environ.get("VMLAYER_TEST_ROOT", Path.home() / ".vmlayer-tests"))
    tmp = Path(tempfile.mkdtemp(prefix="cred-", dir=root))
    p = Proxied(tmp)
    try:
        yield p
    finally:
        for sb in p.client.list_sandboxes():
            sb.kill()
        p.proc.terminate()
        p.proc.wait(timeout=15)
        p.upstream.shutdown()
        if os.environ.get("VMLAYER_KEEP_TEST_DATA") != "1":
            shutil.rmtree(tmp, ignore_errors=True)


def test_sandboxes_hold_placeholders_and_the_upstream_gets_the_secret(proxied: Proxied) -> None:
    with Sandbox.create(IMAGE, credentials=["echo"], client=proxied.client) as sb:
        env = sb.exec("env").stdout
        assert SECRET not in env
        placeholder = sb.exec("printenv ECHO_API_KEY").stdout.strip()
        assert placeholder.startswith("vmlayer-")
        assert f"ECHO_BASE_URL=http://vmlayer.host:{proxied.port}/echo" in env

        r = sb.exec(["python3", "-c", CALL])
        assert r.ok, r.output
        seen = json.loads(r.stdout)
        assert seen["path"] == "/v1/models?limit=2"
        assert seen["body"] == '{"q": 1}'
        headers = {k.lower(): v for k, v in seen["headers"].items()}
        assert headers["authorization"] == f"Bearer {SECRET}"
        assert headers["x-app"] == "yes"
        assert placeholder not in json.dumps(seen)

        # Forks keep the grant (same placeholder in their environment).
        if sb.capabilities.fork:
            (child,) = sb.fork()
            with child:
                assert json.loads(child.exec(["python3", "-c", CALL]).stdout)["path"]

        # Without the placeholder, or for an unknown credential: refused.
        assert proxied.from_host("not-a-placeholder") == 401
        assert proxied.from_host(placeholder) == 200
    # A placeholder dies with its sandbox.
    assert proxied.from_host(placeholder) == 401


def test_unknown_credentials_are_refused(proxied: Proxied) -> None:
    with pytest.raises(vmlayer.InvalidArgument, match="no credential named"):
        Sandbox.create(IMAGE, credentials=["nope"], client=proxied.client)
