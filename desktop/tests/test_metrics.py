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


def test_buffered_timestamp_is_creation_time_and_schema_is_private(tmp_path):
    from datetime import UTC, datetime

    path = tmp_path / "metrics.jsonl"
    metrics = Metrics(path)

    async def behavior():
        before = datetime.now(UTC)
        await metrics.record("inference", time.monotonic(), "ok", buffered=True)
        after = datetime.now(UTC)
        await metrics.flush()
        return before, after

    before, after = asyncio.run(behavior())
    row = json.loads(path.read_text())
    assert set(row) == {"timestamp", "event", "total_ms", "outcome"}
    assert before <= datetime.fromisoformat(row["timestamp"]) <= after
    assert path.stat().st_mode & 0o777 == 0o600


def test_rotation_retains_bounded_json_lines(tmp_path):
    path = tmp_path / "metrics.jsonl"
    metrics = Metrics(path)
    padding = json.dumps({"padding": "x" * (5 * 1024 * 1024 - 20)}) + "\n"

    async def behavior():
        for _ in range(3):
            path.write_text(padding)
            await metrics.record("inference", time.monotonic(), "ok")

    asyncio.run(behavior())
    files = [path, tmp_path / "metrics.jsonl.1", tmp_path / "metrics.jsonl.2"]
    assert all(file.exists() and file.stat().st_size <= 5 * 1024 * 1024 for file in files)
    assert all(json.loads(line) for file in files for line in file.read_text().splitlines())
    assert not (tmp_path / "metrics.jsonl.3").exists()


def test_append_failure_preserves_existing_records(tmp_path, monkeypatch):
    path = tmp_path / "metrics.jsonl"
    path.write_text('{"previous": true}\n')
    metrics = Metrics(path)

    def fail(*_args, **_kwargs):
        raise OSError("disk unavailable")

    monkeypatch.setattr("thalvi_desktop.metrics.os.open", fail)
    with pytest.raises(OSError, match="disk unavailable"):
        asyncio.run(metrics.record("inference", time.monotonic(), "ok"))
    assert path.read_text() == '{"previous": true}\n'


def test_existing_oversized_history_is_preserved_as_a_backup(tmp_path):
    path = tmp_path / "metrics.jsonl"
    previous = json.dumps({"legacy": "x" * (5 * 1024 * 1024)}) + "\n"
    path.write_text(previous)

    asyncio.run(Metrics(path).record("inference", time.monotonic(), "ok"))

    assert path.with_name("metrics.jsonl.1").read_text() == previous
    assert json.loads(path.read_text())["event"] == "inference"
    assert path.stat().st_size < 5 * 1024 * 1024
