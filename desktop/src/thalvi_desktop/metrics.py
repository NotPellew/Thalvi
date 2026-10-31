"""Owns the opt-in latency and outcome log shared by desktop actions and transports."""

import asyncio
import json
import time
from pathlib import Path


class Metrics:
    def __init__(self, path: Path | None):
        self.path = path
        self._pending: list[str] = []

    async def record(
        self, event: str, started: float, outcome: str, *, buffered: bool = False
    ) -> None:
        if self.path is None:
            return
        line = json.dumps(
            {
                "event": event,
                "total_ms": round((time.monotonic() - started) * 1000, 3)
                if buffered
                else round((time.monotonic() - started) * 1000),
                "outcome": outcome,
            }
        )

        if buffered:
            self._pending.append(line + "\n")
            if len(self._pending) >= 125:
                await self.flush()
        else:
            await asyncio.to_thread(self._append, line + "\n")

    async def flush(self) -> None:
        if self.path is None or not self._pending:
            return
        pending, self._pending = self._pending, []
        await asyncio.to_thread(self._append, "".join(pending))

    def _append(self, text: str) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write(text)
