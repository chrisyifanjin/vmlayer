"""Harbor environment backed by vmlayer sandboxes.

`Harbor <https://github.com/harbor-framework/harbor>`_ runs agents against
tasks (Terminal-Bench and many other datasets) inside an *environment*. This
module is one: every trial gets a vmlayer sandbox, on Firecracker where the
host has KVM, else on Docker. Point Harbor at it by import path::

    harbor run --dataset terminal-bench@2.0 --agent claude-code \\
        --environment-import-path vmlayer.integrations.harbor:VmlayerEnvironment \\
        --ek endpoint=tls://10.0.0.5:7443 --ek backend=firecracker

Options (``--ek name=value``):

* ``endpoint``: the daemon (default ``VMLAYER_ENDPOINT`` or the local socket).
  The token and TLS files come from ``VMLAYER_TOKEN`` and ``VMLAYER_TLS_*``.
* ``backend``: ``auto`` (default), ``firecracker`` or ``docker``.
* ``build``: build tasks that ship only a Dockerfile with the local
  ``docker build`` (default true). The image then exists only in the local
  Docker engine, which the Docker backend can use directly. For Firecracker,
  publish the image and set ``docker_image`` in the task instead.
* ``keep``: leave the sandbox running after the trial (default false), for
  inspection; it is killed when its TTL ends.

What maps where:

* The task's ``docker_image`` (or the built Dockerfile) is the sandbox image,
  with no workspace volume, so the image's own working directory is intact.
* ``cpus``, ``memory_mb`` and ``storage_mb`` become the sandbox shape.
* Network policies: ``no-network`` removes the network device, ``allowlist``
  becomes the sandbox's egress allowlist (hostnames, ``*.domain``, IPv4
  addresses and networks; Firecracker only), ``public`` is open egress with
  cloud metadata blocked.
* ``exec(user=...)`` runs as that user; timeouts raise like Harbor's Docker
  environment.

Requires Python 3.12+ (Harbor's own requirement) and ``pip install harbor``.
"""

from __future__ import annotations

import asyncio
import hashlib
import math
import shutil
from datetime import timedelta
from pathlib import Path
from typing import Any

from harbor.environments.base import BaseEnvironment, ExecResult
from harbor.environments.capabilities import EnvironmentCapabilities
from harbor.environments.options import EnvironmentOptions
from harbor.models.task.config import NetworkMode
from pydantic import Field

from .. import Client
from ..aio import AsyncClient, AsyncSandbox
from ..errors import ExecTimeout
from ..types import Backend

# Long enough for any trial; a harness that dies without stopping the
# environment still leaves nothing behind for good.
SANDBOX_TTL = timedelta(hours=12)


class VmlayerEnvironmentOptions(EnvironmentOptions):  # type: ignore[misc]
    endpoint: str | None = Field(default=None, description="vmlayerd endpoint")
    backend: str = Field(default="auto", description="auto, firecracker or docker")
    build: bool = Field(default=True, description="docker build Dockerfile-only tasks locally")
    keep: bool = Field(default=False, description="leave the sandbox running after stop")


class VmlayerEnvironment(BaseEnvironment):  # type: ignore[misc]
    """A Harbor environment whose container is a vmlayer sandbox."""

    options_model = VmlayerEnvironmentOptions
    options: VmlayerEnvironmentOptions

    @staticmethod
    def type() -> str:
        return "vmlayer"

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self._sandbox: AsyncSandbox | None = None
        self._client: AsyncClient | None = None
        self._caps: EnvironmentCapabilities | None = None
        super().__init__(*args, **kwargs)

    # -- what this environment can do -----------------------------------------

    @property
    def capabilities(self) -> EnvironmentCapabilities:
        """Asks the daemon once whether the chosen backend enforces egress
        allowlists (Firecracker does, Docker does not)."""
        if self._caps is None:
            info = Client(self.options.endpoint).info()
            name = self.options.backend
            if name == "auto":
                name = info.default_backend
            backend = next((b for b in info.backends if b.name == name), None)
            allowlist = bool(backend and backend.capabilities.egress_policy)
            self._caps = EnvironmentCapabilities(
                disable_internet=True,
                network_allowlist=allowlist,
                network_allowlist_hostnames=allowlist,
                network_allowlist_wildcard_hostnames=allowlist,
                network_allowlist_ipv4_addresses=allowlist,
                network_allowlist_ipv4_cidrs=allowlist,
            )
        return self._caps

    @property
    def _dockerfile(self) -> Path:
        return Path(self.environment_dir) / "Dockerfile"

    def _validate_definition(self) -> None:
        if self.task_env_config.docker_image:
            return
        if not self._dockerfile.exists():
            raise FileNotFoundError(
                f"{self._dockerfile} not found and the task sets no docker_image"
            )
        if not self.options.build:
            raise ValueError(
                "the task has only a Dockerfile and build=false: publish the image "
                "and set docker_image in the task, or allow build=true"
            )

    # -- lifecycle --------------------------------------------------------------

    @property
    def sandbox(self) -> AsyncSandbox:
        if self._sandbox is None:
            raise RuntimeError("the environment is not started")
        return self._sandbox

    async def _image(self, force_build: bool) -> str:
        if self.task_env_config.docker_image:
            return str(self.task_env_config.docker_image)
        if shutil.which("docker") is None:
            raise RuntimeError("building the task's Dockerfile needs the docker CLI")
        digest = hashlib.sha256(self._dockerfile.read_bytes()).hexdigest()[:12]
        tag = f"vmlayer-harbor/{self.environment_name.lower()}:{digest}"
        if not force_build:
            probe = await asyncio.create_subprocess_exec(
                "docker", "image", "inspect", tag,
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
            )  # fmt: skip
            if await probe.wait() == 0:
                return tag
        build = await asyncio.create_subprocess_exec(
            "docker", "build", "-t", tag, str(self.environment_dir),
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
        )  # fmt: skip
        out, _ = await build.communicate()
        if build.returncode != 0:
            raise RuntimeError(f"docker build failed:\n{out.decode(errors='replace')[-4000:]}")
        return tag

    async def start(self, force_build: bool) -> None:
        image = await self._image(force_build)
        policy = self.network_policy
        cpus = self._effective_cpus
        self._client = AsyncClient(self.options.endpoint)
        self._sandbox = await AsyncSandbox.create(
            image,
            cpus=math.ceil(cpus) if cpus else None,
            memory_mb=self._effective_memory_mb,
            disk_mb=self._effective_storage_mb,
            env=self._startup_env(),
            workdir=self.task_env_config.workdir,
            workspace=None,
            entrypoint=None,
            timeout=SANDBOX_TTL,
            backend=Backend(self.options.backend),
            network=policy.network_mode != NetworkMode.NO_NETWORK,
            egress=list(policy.allowed_hosts)
            if policy.network_mode == NetworkMode.ALLOWLIST
            else None,
            labels={"harbor.session": self.session_id[:63]},
            client=self._client,
        )
        await self.ensure_dirs(self._mount_targets(writable_only=True))
        await self._upload_environment_dir_after_start()

    async def stop(self, delete: bool) -> None:
        if self._sandbox is not None and not self.options.keep:
            await self._sandbox.kill()
        self._sandbox = None
        if self._client is not None:
            await self._client.close()
            self._client = None

    # -- commands and files -----------------------------------------------------

    async def exec(
        self,
        command: str,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        timeout_sec: int | None = None,
        user: str | int | None = None,
    ) -> ExecResult:
        resolved = self._resolve_user(user)
        try:
            r = await self.sandbox.exec(
                ["bash", "-c", command] if await self._has_bash() else command,
                cwd=cwd or self.task_env_config.workdir,
                env=self._merge_env(env),
                timeout=timedelta(seconds=timeout_sec) if timeout_sec else None,
                user=str(resolved) if resolved is not None else None,
            )
        except ExecTimeout:
            raise RuntimeError(f"Command timed out after {timeout_sec} seconds") from None
        return ExecResult(stdout=r.stdout, stderr=r.stderr, return_code=r.exit_code)

    async def _has_bash(self) -> bool:
        # Harbor's commands are written for bash (as `docker exec ... bash -c`);
        # images without it get /bin/sh.
        if not hasattr(self, "_bash"):
            self._bash = await self.sandbox.files.exists("/bin/bash")
        return self._bash

    async def upload_file(self, source_path: Path | str, target_path: str) -> None:
        await self.sandbox.files.upload(source_path, target_path)

    async def upload_dir(self, source_dir: Path | str, target_dir: str) -> None:
        await self.sandbox.files.upload(source_dir, target_dir)

    async def download_file(self, source_path: str, target_path: Path | str) -> None:
        await self.sandbox.files.download(source_path, target_path)

    async def download_dir(self, source_dir: str, target_dir: Path | str) -> None:
        await self.sandbox.files.download(source_dir, target_dir)
