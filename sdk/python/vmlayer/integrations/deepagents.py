"""Deep Agents sandbox backend: keep the agent loop outside the microVM.

Requires ``pip install vmlayer[deepagents]``. Pass this backend to
``create_deep_agent(backend=...)``; Deep Agents then routes its execute and
filesystem tools through vmlayer. ``backend.sandbox`` exposes snapshots and
volumes for callers that need them.
"""

from __future__ import annotations

from datetime import timedelta

from deepagents.backends.protocol import ExecuteResponse, FileDownloadResponse, FileUploadResponse
from deepagents.backends.sandbox import BaseSandbox

from ..client import Client
from ..errors import ExecTimeout, NotFound, VmlayerError
from ..sandbox import Sandbox
from ..types import Backend


class VmlayerSandbox(BaseSandbox):
    """Host-side Deep Agents backend with a vmlayer sandbox as its tool target."""

    def __init__(
        self,
        image: str = "python:3.12-alpine",
        *,
        backend: str = "auto",
        snapshot_id: str | None = None,
        memory_mb: int | None = None,
        timeout: int = 120,
        max_output_bytes: int = 100_000,
        client: Client | None = None,
    ) -> None:
        if timeout <= 0 or max_output_bytes <= 0:
            raise ValueError("timeout and max_output_bytes must be positive")
        self._timeout = timeout
        self._max_output_bytes = max_output_bytes
        if snapshot_id:
            self.sandbox = Sandbox.restore(snapshot_id, client=client)
        else:
            self.sandbox = Sandbox.create(
                image,
                backend=Backend(backend),
                memory_mb=memory_mb,
                entrypoint=None,
                labels={"created-by": "deepagents"},
                client=client,
            )

    @property
    def id(self) -> str:
        return self.sandbox.id

    def execute(self, command: str, *, timeout: int | None = None) -> ExecuteResponse:
        if not command:
            return ExecuteResponse(output="Error: Command must be non-empty.", exit_code=1)
        seconds = self._timeout if timeout is None else timeout
        if seconds <= 0:
            raise ValueError("timeout must be positive")
        try:
            result = self.sandbox.exec(command, timeout=timedelta(seconds=seconds))
            output, code = result.output, result.exit_code
        except ExecTimeout as exc:
            output, code = exc.partial.output + f"\nTimeout after {seconds}s", 124
        except VmlayerError as exc:
            output, code = f"Error: {exc}", 1
        raw = output.encode("utf-8")
        truncated = len(raw) > self._max_output_bytes
        if truncated:
            output = raw[: self._max_output_bytes].decode("utf-8", errors="replace")
        return ExecuteResponse(output=output, exit_code=code, truncated=truncated)

    def upload_files(self, files: list[tuple[str, bytes]]) -> list[FileUploadResponse]:
        responses = []
        for path, content in files:
            try:
                self.sandbox.files.write(path, content)
                responses.append(FileUploadResponse(path=path))
            except VmlayerError as exc:
                responses.append(FileUploadResponse(path=path, error=str(exc)))
        return responses

    def download_files(self, paths: list[str]) -> list[FileDownloadResponse]:
        responses = []
        for path in paths:
            try:
                content = self.sandbox.files.read(path)
                responses.append(FileDownloadResponse(path=path, content=content))
            except NotFound:
                responses.append(FileDownloadResponse(path=path, error="file_not_found"))
            except VmlayerError as exc:
                responses.append(FileDownloadResponse(path=path, error=str(exc)))
        return responses

    def close(self) -> None:
        self.sandbox.kill()

    def __enter__(self) -> VmlayerSandbox:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
