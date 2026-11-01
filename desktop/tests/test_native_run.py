"""Exercises the native entry point with fake devices and speech endpoints."""

import asyncio
import json
import threading
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import pytest

from thalvi_desktop import cli
from thalvi_desktop.config import Config
from thalvi_desktop.cues import Cue, Cues
from thalvi_desktop.workflow import PreviewHost


def test_pause_closes_audio_and_joins_old_wake_work_before_resume(monkeypatch, tmp_path):
    async def behavior():
        loop = asyncio.get_running_loop()
        entered = asyncio.Event()
        closed = asyncio.Event()
        release = threading.Event()

        class Wake:
            running = False
            history = "fresh"

            def detect(self, frame):
                self.running = True
                loop.call_soon_threadsafe(entered.set)
                try:
                    assert release.wait(2)
                    self.history = "old-frame"
                    return None
                finally:
                    self.running = False

            def reset(self):
                assert not self.running
                self.history = "fresh"

        class Audio:
            def __init__(self, _device):
                pass

            async def __aenter__(self):
                return self

            async def __aexit__(self, *_args):
                closed.set()

            async def next(self):
                return bytes(7680)

        wake = Wake()
        resident = SimpleNamespace(
            wake=wake, host=PreviewHost(), notice=lambda _: None, state=lambda *args: None
        )
        monkeypatch.setattr(cli, "Audio", Audio)
        monkeypatch.setattr(cli, "notice", lambda _: None)
        config = replace(
            Config.load(Path(__file__).parents[1] / "config.example.json"),
            metrics=tmp_path / "metrics.jsonl",
        )
        task = asyncio.create_task(cli._run(config, False, False, resident))
        try:
            await asyncio.wait_for(entered.wait(), 1)
            task.cancel()
            await asyncio.wait_for(closed.wait(), 1)
            assert wake.running
            assert not task.done()
            release.set()
            with pytest.raises(asyncio.CancelledError):
                await asyncio.wait_for(task, 1)
            await asyncio.to_thread(wake.reset)
            assert wake.history == "fresh"
            records = [json.loads(line) for line in config.metrics.read_text().splitlines()]
            assert all(row["event"] != "desktop_wake_load" for row in records)
        finally:
            release.set()
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())


def wake_frame(level, index):
    if isinstance(level, int):
        value = level
    elif level == "pulse":
        value = -32768 if 2 <= index <= 12 else 0
    else:
        value = 4000 if level == "activity" else 0
    return value.to_bytes(2, "little", signed=True) * 3840


@pytest.mark.parametrize(
    "mode, failure, wake_frames, level",
    [
        ("capture", None, 2, "quiet"),
        ("conversation", None, 2, "activity"),
        ("capture", "flow", 2, "quiet"),
        ("conversation", "flow", 2, "quiet"),
        ("capture", "detect", 2, "quiet"),
        ("capture", "cancel", 2, "quiet"),
        ("capture", None, 125, "pulse"),
        ("capture", None, 2, 999),
        ("capture", None, 2, -999),
        ("capture", None, 2, 1000),
        ("capture", None, 2, -1000),
    ],
)
def test_native_entry_routes_wakes_recovers_and_releases_audio(
    monkeypatch, tmp_path, mode, failure, wake_frames, level
):
    config = replace(
        Config.load(Path(__file__).parents[1] / "config.example.json"),
        metrics=tmp_path / "metrics.jsonl",
    )
    events, notices = [], []
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
            assert len(frame) == 7680
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
            self.calls = 0

        async def __aenter__(self):
            events.append("opened")
            return self

        async def __aexit__(self, *_args):
            events.append("closed")
            closed.set()

        async def next(self):
            self.calls += 1
            return wake_frame(level, self.calls)

        def set_input(self, enabled):
            if enabled:
                events.append("discard")

        async def play_cue(self, cue):
            events.append(f"cue: {cue.value}")
            if cue == Cue.WAKE:
                recording.set()

    class Host(PreviewHost):
        def capture(self):
            events.append("target")
            return super().capture()

    host = Host(lambda _: None)

    async def capture(_config, _audio, current, _notice, **_kwargs):
        assert events.count("detect") == wake_frames
        events.append("capture")
        await _audio.play_cue(Cue.WAKE)
        if failure == "flow":
            current.final("Keep this partial dictation")
            raise RuntimeError("speech failed")
        return current.final("A test dictation paste that")

    async def conversation(_config, _audio, _notice):
        assert events.count("detect") == wake_frames
        events.append("conversation")
        if failure == "flow":
            raise RuntimeError("conversation failed")

    def notice(message):
        notices.append(message)
        events.append(f"notice: {message}")

    monkeypatch.setattr(cli, "Wake", Wake)
    monkeypatch.setattr(cli, "Audio", Audio)
    monkeypatch.setattr(cli, "desktop", lambda: host)
    monkeypatch.setattr(cli, "capture", capture)
    monkeypatch.setattr(cli, "conversation", conversation)
    monkeypatch.setattr(cli, "notice", notice)

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
        assert events.count("reset") == events.count("discard") == 0

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
    if mode == "capture" and failure not in {"detect", "cancel"}:
        recognized = "notice: Capture waking. Reading the target window."
        assert events.index(recognized) < events.index("target")
    if mode == "conversation" and failure not in {"detect", "cancel"}:
        recognized = "notice: Conversation waking. Use headphones in this native prototype."
        assert events.index(recognized) < events.index("conversation")

    if level == "pulse":
        expected_levels = ["quiet", "activity", "quiet", "quiet", "quiet", "quiet"]
    else:
        active = level == "activity" or (isinstance(level, int) and abs(level) >= 1000)
        expected_levels = ["activity" if active else "quiet"]
    expected_notices = [
        f"Sampled wake audio: {value}."
        for index, value in enumerate(expected_levels)
        if index == 0 or value != expected_levels[index - 1]
    ]
    assert [m for m in notices if m.startswith("Sampled wake audio:")] == expected_notices

    rows = [json.loads(line) for line in config.metrics.read_text().splitlines()]
    inference = [row for row in rows if row["event"] == "desktop_wake_inference"]
    assert len(inference) == wake_frames
    expected = {"detect": "error", "cancel": "cancelled"}.get(
        failure, {"capture": "capture", "conversation": "conversation"}[mode]
    )
    assert [row["outcome"] for row in inference] == ["none"] * (wake_frames - 1) + [expected]
    levels = [row for row in rows if row["event"] == "desktop_wake_level"]
    assert [row["outcome"] for row in levels] == expected_levels
    targets = [row for row in rows if row["event"] == "desktop_target_capture"]
    assert len(targets) == int(mode == "capture" and failure not in {"detect", "cancel"})
    assert all(row["outcome"] == "ok" for row in targets)
    assert all(set(row) == {"timestamp", "event", "total_ms", "outcome"} for row in rows)
    assert "dictation" not in config.metrics.read_text()


@pytest.mark.parametrize("once", [True, False])
def test_delivery_has_no_postdelivery_cue_and_idle_notice_is_truthful(monkeypatch, once):
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
            return wake_frame("activity", self.calls)

        def set_input(self, _enabled):
            pass

        async def play_cue(self, _cue):
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

    def unexpected_level(_frame):
        raise AssertionError("Disabled diagnostics must not inspect input levels.")

    monkeypatch.setattr(cli, "wake_level", unexpected_level)
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
    assert not any("action outcome is unchanged" in message for message in notices)
    assert not any("Nothing will be delivered automatically" in message for message in notices)
    assert config.metrics is None
    assert not any(message.startswith("Sampled wake audio:") for message in notices)


@pytest.mark.parametrize("reset_fails", [False, True])
def test_continuous_reset_gates_audio_and_preserves_delivery(monkeypatch, tmp_path, reset_fails):
    config = replace(
        Config.load(Path(__file__).parents[1] / "config.example.json"),
        metrics=tmp_path / "metrics.jsonl",
    )
    events, notices = [], []

    class Wake:
        def __init__(self, _config):
            pass

        def detect(self, _frame):
            return "capture"

        def reset(self):
            events.append("reset")
            if reset_fails:
                raise RuntimeError("reset failed")

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
            return wake_frame("quiet", 1)

        def set_input(self, enabled):
            events.append(enabled)

    host = PreviewHost(lambda _: None)

    async def capture(_config, _audio, current, _notice, **_kwargs):
        return current.final("hello paste that")

    monkeypatch.setattr(cli, "Wake", Wake)
    monkeypatch.setattr(cli, "Audio", Audio)
    monkeypatch.setattr(cli, "desktop", lambda: host)
    monkeypatch.setattr(cli, "capture", capture)
    monkeypatch.setattr(cli, "notice", notices.append)
    if reset_fails:
        assert asyncio.run(cli.run(config, once=False, dry_run=False)) == 1
    else:
        with pytest.raises(asyncio.CancelledError):
            asyncio.run(cli.run(config, once=False, dry_run=False))
    assert events == [False, False, "reset", *([] if reset_fails else [True]), "released"]
    assert host.operations == [("deliver", "hello", False)]
    assert sum(n.startswith("Waiting for a wake word") for n in notices) == (
        1 if reset_fails else 2
    )
    assert not any("Nothing will be delivered" in n for n in notices)
    rows = [json.loads(line) for line in config.metrics.read_text().splitlines()]
    assert [row["outcome"] for row in rows if row["event"] == "desktop_wake_reset"] == [
        "error" if reset_fails else "ok"
    ]


@pytest.mark.parametrize("gap", [False, True])
def test_ambiguous_wake_notifies_once_until_it_clears_without_action(monkeypatch, gap):
    config = Config.load(Path(__file__).parents[1] / "config.example.json")
    notices, events = [], []
    modes = ["ambiguous"] * 25 + ([None] + ["ambiguous"] * 25 if gap else [])

    class Wake:
        def __init__(self, _config):
            self.modes = iter(modes)

        def detect(self, _frame):
            return next(self.modes)

    class Audio:
        def __init__(self, _device):
            self.calls = 0

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            events.append("released")

        async def next(self):
            self.calls += 1
            if self.calls > len(modes):
                raise asyncio.CancelledError
            return wake_frame("quiet", 1)

    host = PreviewHost(lambda _: None)
    monkeypatch.setattr(cli, "Wake", Wake)
    monkeypatch.setattr(cli, "Audio", Audio)
    monkeypatch.setattr(cli, "desktop", lambda: host)
    monkeypatch.setattr(cli, "notice", notices.append)
    with pytest.raises(asyncio.CancelledError):
        asyncio.run(cli.run(config, once=False, dry_run=False))
    assert host.operations == [] and events == ["released"]
    assert sum("ambiguous" in notice for notice in notices) == 1 + int(gap)


@pytest.mark.parametrize("error_cue", [True, False])
@pytest.mark.parametrize("failure", ["flow", "unsuccessful"])
def test_flow_failures_play_the_error_cue_after_telling_the_user(monkeypatch, failure, error_cue):
    config = replace(
        Config.load(Path(__file__).parents[1] / "config.example.json"),
        cues=Cues(error=error_cue),
    )
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
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            pass

        async def next(self):
            return wake_frame("quiet", 1)

        def set_input(self, _enabled):
            pass

        async def play_cue(self, cue):
            events.append(cue)
            raise RuntimeError("speaker failed")

    class Host(PreviewHost):
        def copy(self, text):
            events.append("clipboard")

    async def capture(_config, _audio, current, _notice, **_kwargs):
        current.final("Keep this partial dictation")
        if failure == "flow":
            raise RuntimeError("speech failed")
        return current.final("paste that")

    async def finish(*_args):
        return False

    monkeypatch.setattr(cli, "Wake", Wake)
    monkeypatch.setattr(cli, "Audio", Audio)
    monkeypatch.setattr(cli, "desktop", lambda: Host(lambda _: None))
    monkeypatch.setattr(cli, "capture", capture)
    monkeypatch.setattr(cli.Workflow, "finish", lambda *_args: finish())
    monkeypatch.setattr(cli, "notice", lambda message: events.append(f"notice: {message}"))

    assert asyncio.run(cli.run(config, once=True, dry_run=False)) == 1
    told = [e for e in events if isinstance(e, str) and e.startswith("notice: speech failed")]
    assert bool(told) == (failure == "flow")
    played = [e for e in events if isinstance(e, Cue)]
    assert played == ([Cue.ERROR] if error_cue else [])
    if failure == "flow":
        order = [e for e in events if e in (told[0], Cue.ERROR, "clipboard")]
        assert order == [told[0], *([Cue.ERROR] if error_cue else []), "clipboard"]
