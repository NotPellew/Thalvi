"""Checks bounded inference logging without audio or captured text."""

import asyncio
import json
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
        assert path.exists() is (count == 125)
        await metrics.flush()
        await metrics.flush()

    asyncio.run(behavior())
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    assert len(rows) == count
    assert all(row["event"] == "inference" and row["outcome"] == "ok" for row in rows)
    assert all(row["total_ms"] >= 0 for row in rows)


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
