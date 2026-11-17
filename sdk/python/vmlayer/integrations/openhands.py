"""OpenHands ``RemoteWorkspace`` backed by a vmlayer sandbox.

Requires Python 3.12+ and ``pip install vmlayer[openhands]``. The workspace
uses the same OpenHands agent-server image and HTTP API as DockerWorkspace.
``workspace.sandbox`` exposes vmlayer snapshots, volumes, and fork.
"""

from __future__ import annotations

import os
import time
from datetime import timedelta
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

from openhands.sdk.workspace import RemoteWorkspace
from pydantic import Field, PrivateAttr

from ..client import Client
from ..sandbox import Sandbox
from ..types import Backend


class VmlayerWorkspace(RemoteWorkspace):  # type: ignore[misc]
    """Run the OpenHands agent server in Firecracker or another vmlayer backend."""

    working_dir: str = Field(default="/workspace")
    host: str = Field(default="")
    server_image: str = Field(default="ghcr.io/openhands/agent-server:latest-python")
    backend: str = Field(default="auto")
    endpoint: str | None = Field(default=None)
    snapshot_id: str | None = Field(default=None)
    memory_mb: int = Field(default=2048, gt=0)
    health_check_timeout: float = Field(default=180.0, gt=0)
    sandbox_timeout: int = Field(default=3600, ge=0)
    forward_env: list[str] = Field(
        default_factory=lambda: ["DEBUG", "SESSION_API_KEY", "OH_SESSION_API_KEYS_0"]
    )

    _sandbox: Sandbox | None = PrivateAttr(default=None)

    @property
    def sandbox(self) -> Sandbox:
        """The running sandbox, for snapshots and other vmlayer operations."""
        if self._sandbox is None:
            raise RuntimeError("workspace is closed")
        return self._sandbox

    def model_post_init(self, context: Any) -> None:
        env = {key: os.environ[key] for key in self.forward_env if key in os.environ}
        client = Client(self.endpoint) if self.endpoint else None
        try:
            if self.snapshot_id:
                self._sandbox = Sandbox.restore(
                    self.snapshot_id,
                    timeout=timedelta(seconds=self.sandbox_timeout)
                    if self.sandbox_timeout
                    else None,
                    labels={"created-by": "openhands"},
                    client=client,
                )
            else:
                self._sandbox = Sandbox.create(
                    self.server_image,
                    backend=Backend(self.backend),
                    memory_mb=self.memory_mb,
                    workdir=self.working_dir,
                    env=env,
                    entrypoint=[
                        "tini",
                        "--",
                        "/usr/local/bin/openhands-agent-server",
                        "--host",
                        "0.0.0.0",
                        "--port",
                        "8000",
                    ],
                    timeout=timedelta(seconds=self.sandbox_timeout)
                    if self.sandbox_timeout
                    else None,
                    labels={"created-by": "openhands"},
                    client=client,
                )
            self._sandbox.wait_for_port(8000, timeout=timedelta(seconds=self.health_check_timeout))
            self.host = self._sandbox.expose(8000).url
            deadline = time.monotonic() + self.health_check_timeout
            while True:
                try:
                    with urlopen(f"{self.host}/health", timeout=2) as response:
                        if 200 <= response.status < 300:
                            break
                except (HTTPError, URLError, TimeoutError, OSError):
                    pass
                if time.monotonic() >= deadline:
                    raise TimeoutError("OpenHands agent server did not become healthy")
                time.sleep(0.25)
            self.api_key = os.environ.get(
                "OH_SESSION_API_KEYS_0", os.environ.get("SESSION_API_KEY")
            )
            super().model_post_init(context)
        except BaseException:
            self.cleanup()
            raise

    def cleanup(self) -> None:
        """Close the HTTP client and kill the sandbox. Safe to call twice."""
        self.reset_client()
        sandbox, self._sandbox = self._sandbox, None
        if sandbox is not None:
            sandbox.kill()

    def __exit__(self, exc_type: type | None, exc_val: BaseException | None, exc_tb: Any) -> None:
        try:
            super().__exit__(exc_type, exc_val, exc_tb)
        finally:
            self.cleanup()
