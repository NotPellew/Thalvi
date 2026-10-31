"""Exercises the native entry point with fake devices and speech endpoints."""

import asyncio
import json
from dataclasses import replace
from pathlib import Path

import pytest

from thalvi_desktop import cli
from thalvi_desktop.config import Config
from thalvi_desktop.workflow import PreviewHost


@pytest.mark.parametrize(
    "mode, failure",
    [
        ("capture", None),
        ("conversation", None),
        ("capture", "flow"),
        ("conversation", "flow"),
        ("capture", "detect"),
        ("capture", "cancel"),
    ],
)
def test_native_entry_routes_wakes_recovers_and_releases_audio(
    monkeypatch, tmp_path, mode, failure
):
    config = replace(
        Config.load(Path(__file__).parents[1] / "config.example.json"),
        metrics=tmp_path / "metrics.jsonl",
    )
    events = []

    class Wake:
        def __init__(self, _config):
            self.frames = 0

        def detect(self, frame):
            assert frame == bytes(7680)
            self.frames += 1
            events.append("detect")
            if self.frames == 1:
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

        async def next(self):
            return bytes(7680)

        def discard(self):
            events.append("discard")

    class Host(PreviewHost):
        def capture(self):
            events.append("target")
            return super().capture()

    host = Host(lambda _: None)

    async def capture(_config, _audio, current, _notice):
        assert events.count("detect") == 2
        events.append("capture")
        if failure == "flow":
            current.final("Keep this partial dictation")
            raise RuntimeError("speech failed")
        return current.final("A test dictation paste that")

    async def conversation(_config, _audio, _notice):
        assert events.count("detect") == 2
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
        assert asyncio.run(cli.run(config, once=True, dry_run=False)) == int(failure is not None)
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
    assert len(inference) == 2
    assert inference[-1]["outcome"] == {"detect": "error", "cancel": "cancelled"}.get(failure, "ok")
    assert all(set(row) == {"event", "total_ms", "outcome"} for row in rows)
    assert "dictation" not in config.metrics.read_text()
