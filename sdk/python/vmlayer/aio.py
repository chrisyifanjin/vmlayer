"""``asyncio`` twin of the blocking API.

Not implemented in this release. The blocking API is thread-safe and the
daemon serves many concurrent calls, so ``asyncio.to_thread`` is the
interim path; a native ``grpc.aio`` client is planned.
"""

from __future__ import annotations

__all__: list[str] = []
