"""Owns the opt-in latency and outcome log shared by desktop actions and transports."""

import asyncio
import json
import time
from pathlib import Path


class Metrics:
    def __init__(self, path: Path | None):
        self.path = path

    async def record(self, event: str, started: float, outcome: str) -> None:
        if self.path is None:
            return
        line = json.dumps(
            {
                "event": event,
                "total_ms": round((time.monotonic() - started) * 1000),
                "outcome": outcome,
            }
        )

        def write() -> None:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.path.open("a", encoding="utf-8") as stream:
                stream.write(line + "\n")

        await asyncio.to_thread(write)
