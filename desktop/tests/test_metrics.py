"""Checks bounded inference logging without audio or captured text."""

import asyncio
import json
import threading
import time

import pytest

from thalvi_desktop.metrics import Metrics


@pytest.mark.parametrize("count", [1, 125])
def test_buffered_model_timings_flush_at_the_bound_or_explicitly(tmp_path, count):
    path = tmp_path / "metrics.jsonl"
    metrics = Metrics(path)

    async def behavior():
        for _ in range(count):
            await metrics.record("inference", time.monotonic(), "ok", buffered=True)
        assert not path.exists()
        await metrics.flush()
        await metrics.flush()

    asyncio.run(behavior())
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    assert len(rows) == count
    assert all(row["event"] == "inference" and row["outcome"] == "ok" for row in rows)
    assert all(row["total_ms"] >= 0 for row in rows)


def test_slow_writer_keeps_memory_bounded_and_drops_only_optional_metrics(tmp_path):
    path = tmp_path / "metrics.jsonl"
    metrics = Metrics(path)
    started, release = threading.Event(), threading.Event()
    append = metrics._append

    def blocked(text):
        started.set()
        assert release.wait(3)
        append(text)

    metrics._append = blocked

    async def behavior():
        try:
            for _ in range(250):
                await metrics.record("inference", time.monotonic(), "ok", buffered=True)
            assert await asyncio.to_thread(started.wait, 1)
            with pytest.raises(OSError, match="buffer is full"):
                await metrics.record("dropped", time.monotonic(), "ok", buffered=True)
            await metrics.record("dropped", time.monotonic(), "ok", buffered=True)
        finally:
            release.set()
        await metrics.flush()

    asyncio.run(behavior())
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    assert len(rows) == 250
    assert all(row["event"] == "inference" for row in rows)


def test_disabled_metrics_schedule_no_io(monkeypatch):
    metrics = Metrics(None)

    async def unexpected(*_args, **_kwargs):
        raise AssertionError("Disabled metrics must not schedule file writes.")

    monkeypatch.setattr(asyncio, "to_thread", unexpected)

    async def behavior():
        for _ in range(126):
            await metrics.record("inference", time.monotonic(), "ok", buffered=True)
        await metrics.flush()

    asyncio.run(behavior())
