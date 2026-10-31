"""Exercises the native entry point with fake devices and speech endpoints."""

import asyncio
import json
import threading
from dataclasses import replace
from pathlib import Path

import pytest

from thalvi_desktop import cli
from thalvi_desktop.config import Config
from thalvi_desktop.workflow import PreviewHost


@pytest.mark.parametrize(
    "mode, failure, wake_frames",
    [
        ("capture", None, 2),
        ("conversation", None, 2),
        ("capture", "flow", 2),
        ("conversation", "flow", 2),
        ("capture", "detect", 2),
        ("capture", "cancel", 2),
        ("capture", None, 125),
    ],
)
def test_native_entry_routes_wakes_recovers_and_releases_audio(
    monkeypatch, tmp_path, mode, failure, wake_frames
):
    config = replace(
        Config.load(Path(__file__).parents[1] / "config.example.json"),
        metrics=tmp_path / "metrics.jsonl",
    )
    events = []
    recording, closed = asyncio.Event(), asyncio.Event()
    writer_started, release = threading.Event(), threading.Event()
    append = cli.Metrics._append

    def blocked_append(self, text):
        if wake_frames == 125 and "desktop_wake_inference" in text:
            writer_started.set()
            assert release.wait(3)
        append(self, text)

    monkeypatch.setattr(cli.Metrics, "_append", blocked_append)

    class Wake:
        def __init__(self, _config):
            self.frames = 0

        def detect(self, frame):
            assert frame == bytes(7680)
            self.frames += 1
            events.append("detect")
            if self.frames < wake_frames:
                return None
            if failure == "detect":
                raise RuntimeError("detector failed")
            if failure == "cancel":
                raise asyncio.CancelledError
            return mode

        def reset(self):
            events.append("reset")

    class Audio:
        failed = False

        def __init__(self, _device):
            pass

        async def __aenter__(self):
            events.append("opened")
            return self

        async def __aexit__(self, *_args):
            events.append("closed")
            closed.set()

        async def next(self):
            return bytes(7680)

        def set_input(self, enabled):
            if enabled:
                events.append("discard")

        async def cue(self, frequency):
            if frequency == 440:
                recording.set()

    class Host(PreviewHost):
        def capture(self):
            events.append("target")
            return super().capture()

    host = Host(lambda _: None)

    async def capture(_config, _audio, current, _notice, **_kwargs):
        assert events.count("detect") == wake_frames
        events.append("capture")
        await _audio.cue(440)
        if failure == "flow":
            current.final("Keep this partial dictation")
            raise RuntimeError("speech failed")
        return current.final("A test dictation paste that")

    async def conversation(_config, _audio, _notice):
        assert events.count("detect") == wake_frames
        events.append("conversation")
        if failure == "flow":
            raise RuntimeError("conversation failed")

    monkeypatch.setattr(cli, "Wake", Wake)
    monkeypatch.setattr(cli, "Audio", Audio)
    monkeypatch.setattr(cli, "desktop", lambda: host)
    monkeypatch.setattr(cli, "capture", capture)
    monkeypatch.setattr(cli, "conversation", conversation)
    monkeypatch.setattr(cli, "notice", lambda _: None)

    if failure in {"detect", "cancel"}:
        error = RuntimeError if failure == "detect" else asyncio.CancelledError
        with pytest.raises(error):
            asyncio.run(cli.run(config, once=True, dry_run=False))
        assert "capture" not in events and "conversation" not in events
        assert "reset" not in events and "discard" not in events
    else:

        async def execute():
            if wake_frames != 125:
                return await cli.run(config, once=True, dry_run=False)
            pending = asyncio.create_task(cli.run(config, once=True, dry_run=False))
            try:
                assert await asyncio.to_thread(writer_started.wait, 1)
                await asyncio.wait_for(recording.wait(), 1)
                await asyncio.wait_for(closed.wait(), 1)
            finally:
                release.set()
            return await asyncio.wait_for(pending, 2)

        assert asyncio.run(execute()) == int(failure is not None)
        assert events.count(mode) == 1
        assert events.count("reset") == events.count("discard") == 1

    assert events.count("opened") == events.count("closed") == 1
    if mode == "capture" and failure is None:
        assert host.operations == [("deliver", "A test dictation", False)]
        assert events.index("target") > max(
            i for i, event in enumerate(events) if event == "detect"
        )
    elif mode == "capture" and failure == "flow":
        assert host.operations == [("clipboard", "Keep this partial dictation", False)]
    else:
        assert host.operations == []

    rows = [json.loads(line) for line in config.metrics.read_text().splitlines()]
    inference = [row for row in rows if row["event"] == "desktop_wake_inference"]
    assert len(inference) == wake_frames
    assert inference[-1]["outcome"] == {"detect": "error", "cancel": "cancelled"}.get(failure, "ok")
    assert all(set(row) == {"event", "total_ms", "outcome"} for row in rows)
    assert "dictation" not in config.metrics.read_text()


@pytest.mark.parametrize("once", [True, False])
def test_delivery_cue_failure_keeps_success_and_idle_notice_is_truthful(monkeypatch, once):
    config = Config.load(Path(__file__).parents[1] / "config.example.json")
    notices = []
    events = []

    class Wake:
        def __init__(self, _config):
            pass

        def detect(self, _frame):
            return "capture"

        def reset(self):
            pass

    class Audio:
        failed = False

        def __init__(self, _device):
            self.calls = 0

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            events.append("released")

        async def next(self):
            self.calls += 1
            if self.calls > 1:
                raise asyncio.CancelledError
            return bytes(7680)

        def set_input(self, _enabled):
            pass

        async def cue(self, _frequency):
            raise RuntimeError("speaker failed")

    host = PreviewHost(lambda _: None)

    async def capture(_config, _audio, current, _notice, **_kwargs):
        return current.final("hello paste that")

    def notice(message):
        notices.append(message)
        if message.startswith("Finished."):
            assert events == ["released"]

    monkeypatch.setattr(cli, "Wake", Wake)
    monkeypatch.setattr(cli, "Audio", Audio)
    monkeypatch.setattr(cli, "desktop", lambda: host)
    monkeypatch.setattr(cli, "capture", capture)
    monkeypatch.setattr(cli, "notice", notice)
    if once:
        assert asyncio.run(cli.run(config, once=True, dry_run=False)) == 0
        assert any(message.startswith("Finished.") for message in notices)
        assert sum(message.startswith("Waiting for a wake word") for message in notices) == 1
    else:
        with pytest.raises(asyncio.CancelledError):
            asyncio.run(cli.run(config, once=False, dry_run=False))
        assert sum(message.startswith("Waiting for a wake word") for message in notices) == 2
        assert not any(message.startswith("Finished.") for message in notices)
    assert host.operations == [("deliver", "hello", False)]
    assert any("action outcome is unchanged" in message for message in notices)
    assert not any("Nothing will be delivered automatically" in message for message in notices)
