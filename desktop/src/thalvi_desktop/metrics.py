"""Owns the opt-in latency and outcome log shared by desktop actions and transports."""

import asyncio
import json
import os
import threading
import time
from datetime import UTC, datetime
from pathlib import Path

_WRITE_LOCK = threading.Lock()


class Metrics:
    def __init__(self, path: Path | None):
        self.path = path
        self._pending: list[str] = []
        self._writer: asyncio.Task[None] | None = None
        self._overflow_reported = False

    async def record(
        self, event: str, started: float, outcome: str, *, buffered: bool = False
    ) -> None:
        if self.path is None:
            return
        line = json.dumps(
            {
                "timestamp": datetime.now(UTC).isoformat(),
                "event": event,
                "total_ms": round((time.monotonic() - started) * 1000, 3)
                if buffered
                else round((time.monotonic() - started) * 1000),
                "outcome": outcome,
            }
        )

        if buffered:
            self._check_writer()
            if len(self._pending) >= 125:
                if self._writer is None:
                    self._start_write()
                else:
                    if not self._overflow_reported:
                        self._overflow_reported = True
                        raise OSError("The optional metrics buffer is full.")
                    return
            self._overflow_reported = False
            self._pending.append(line + "\n")
            if len(self._pending) >= 125 and self._writer is None:
                self._start_write()
        else:
            await asyncio.to_thread(self._append, line + "\n")

    async def flush(self) -> None:
        while self.path is not None:
            self._check_writer()
            if self._writer is None:
                if not self._pending:
                    return
                self._start_write()
            await asyncio.shield(self._writer)

    def _start_write(self) -> None:
        pending, self._pending = self._pending, []
        self._writer = asyncio.create_task(asyncio.to_thread(self._append, "".join(pending)))

    def _check_writer(self) -> None:
        if self._writer is not None and self._writer.done():
            writer, self._writer = self._writer, None
            writer.result()

    def _append(self, text: str) -> None:
        limit = 5 * 1024 * 1024
        with _WRITE_LOCK:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            for line in text.splitlines(keepends=True):
                data = line.encode("utf-8")
                if len(data) > limit:
                    raise OSError("A metrics record exceeds the log size limit.")
                try:
                    size = self.path.stat().st_size
                except FileNotFoundError:
                    size = 0
                if size + len(data) > limit:
                    first = self.path.with_name(self.path.name + ".1")
                    second = self.path.with_name(self.path.name + ".2")
                    if first.exists():
                        first.replace(second)
                    self.path.replace(first)
                fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
                with os.fdopen(fd, "ab") as stream:
                    stream.write(data)
