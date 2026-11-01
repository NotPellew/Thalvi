"""Exercises the actual native WebSocket transport against local fixtures."""

import asyncio
import json
from dataclasses import replace
from pathlib import Path

import pytest
from websockets.asyncio.server import serve

from thalvi_desktop.capture import Capture, Ending, Request
from thalvi_desktop.client import capture, conversation
from thalvi_desktop.config import Config


class FakeAudio:
    def __init__(self):
        self.queue = asyncio.Queue()
        self.queue.put_nowait(bytes(7680))
        self.played = []
        self.stopped = 0
        self.sent_bytes = self.played_bytes = 0

    def set_input(self, enabled):
        while not self.queue.empty():
            self.queue.get_nowait()
        if enabled:
            self.queue.put_nowait(bytes(7680))

    async def cue(self, _frequency, *, duration=0.12):
        pass

    async def next(self):
        if self.queue.empty() and getattr(self, "quiet_frames", False):
            await asyncio.sleep(0)
            return bytes(7680)
        return await self.queue.get()

    def new_conversation(self):
        self.sent_bytes = self.played_bytes = 0

    def drained_bytes(self):
        return self.played_bytes if self.played_bytes == self.sent_bytes else None

    def play(self, pcm):
        self.played.append(pcm)
        self.sent_bytes += len(pcm)
        self.played_bytes = self.sent_bytes

    def stop(self):
        self.stopped += 1


@pytest.fixture(autouse=True)
def instant_quiet_gate(monkeypatch):
    from thalvi_desktop import client
    from thalvi_desktop.pending import Pending

    class SilentVoice:
        def speech(self, _frame):
            return False

    class TestPending(Pending):
        def __init__(self, capture, _seconds):
            super().__init__(capture, 0)

        def final(self, text, now, *, speech=False):
            result = super().final(text, now, speech=speech)
            if self.request is not None:
                for audio in audio_instances:
                    audio.quiet_frames = True
                    if audio.queue.empty():
                        audio.queue.put_nowait(bytes(7680))
            return result

    audio_instances = []
    original_init = FakeAudio.__init__

    def init(audio):
        original_init(audio)
        audio_instances.append(audio)

    monkeypatch.setattr(FakeAudio, "__init__", init)
    monkeypatch.setattr(client, "VoiceActivity", SilentVoice)
    monkeypatch.setattr(client, "Pending", TestPending)


@pytest.fixture
def config():
    return Config.load(Path(__file__).parents[1] / "config.example.json")


def test_transcription_connection_waits_for_ready_and_buffers_until_ending(config):
    async def behavior():
        received = []

        async def server(socket):
            assert socket.request.path == "/transcription"
            await socket.send(json.dumps({"type": "ready"}))
            received.append(await socket.recv())
            for event in [
                {"type": "partial", "text": "wrong partial must not be delivered"},
                {"type": "final", "text": "First piece."},
                {"type": "final", "text": "Second piece paste that."},
            ]:
                await socket.send(json.dumps(event))
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            result = await asyncio.wait_for(
                capture(local, FakeAudio(), Capture(config.endings), lambda _: None), 2
            )
        assert result == Request(Ending.PASTE, "First piece. Second piece")
        assert received == [bytes(3840)]

    asyncio.run(behavior())


@pytest.mark.parametrize("failure", ["disconnect", "error", "invalid"])
def test_transcription_failure_preserves_accepted_text_without_delivery(config, failure):
    async def behavior():
        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            await socket.send('{"type":"final","text":"Keep this piece"}')
            if failure == "error":
                await socket.send('{"type":"error","message":"recognizer lost"}')
            elif failure == "invalid":
                await socket.send('{"type":"final","text":42}')

        current = Capture(config.endings)
        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            with pytest.raises(RuntimeError):
                await asyncio.wait_for(capture(local, FakeAudio(), current, lambda _: None), 2)
        assert current.text == "Keep this piece"
        assert current.finished is False

    asyncio.run(behavior())


def test_conversation_has_its_own_socket_streams_audio_and_stops_on_voice_command(config):
    async def behavior():
        audio = FakeAudio()

        async def server(socket):
            assert socket.request.path == "/session"
            assert json.loads(await socket.recv()) == {"type": "voice", "on": True}
            await socket.send('{"type":"state","state":"listening"}')
            assert await socket.recv() == bytes(3840)
            await socket.send(b"\0\0" * 100)
            await socket.send('{"type":"reply","text":"Hi."}')
            await socket.send('{"type":"reply_end"}')
            await socket.send('{"type":"interrupted"}')
            await socket.send('{"type":"user","text":"stop conversation"}')
            async for raw in socket:
                if isinstance(raw, str) and json.loads(raw).get("type") == "cancel":
                    break

        notices = []
        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            await asyncio.wait_for(conversation(local, audio, notices.append), 2)
        assert audio.played == [b"\0\0" * 100]
        assert audio.stopped >= 1
        assert "Thalvi · Hi." in notices

    asyncio.run(behavior())


def test_confirmation_discards_old_audio_and_only_accepts_standalone_affirmation(config):
    from thalvi_desktop.capture import Confirmation

    async def behavior():
        audio = FakeAudio()
        ready = asyncio.Event()
        sent = []
        rejected = asyncio.Event()
        release = asyncio.Event()

        def set_input(enabled):
            while not audio.queue.empty():
                audio.queue.get_nowait()

        audio.set_input = set_input

        async def server(socket):
            await socket.send('{"type":"ready"}')
            ready.set()
            sent.append(await socket.recv())
            await socket.send('{"type":"final","text":"not yes"}')
            await release.wait()
            await socket.send('{"type":"final","text":"yes send it"}')
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            prompting = asyncio.Event()
            pending = asyncio.create_task(
                capture(
                    local,
                    audio,
                    Confirmation(),
                    lambda message: (
                        rejected.set() if message.startswith("Capturing ·") else prompting.set()
                    ),
                    confirmation=True,
                )
            )
            await asyncio.wait_for(ready.wait(), 1)
            await asyncio.wait_for(prompting.wait(), 1)
            audio.queue.put_nowait(bytes([2, 0]) * 3840)
            await asyncio.wait_for(rejected.wait(), 1)
            assert not pending.done()
            release.set()
            assert await asyncio.wait_for(pending, 2) == Request(Ending.SUBMIT, "")
        assert sent == [bytes([2, 0]) * 1920]

    asyncio.run(behavior())


def test_capture_discards_audio_until_ready_tone_finishes(config):
    async def behavior():
        connected, ready_allowed, cue_started, cue_allowed, receiving = (
            asyncio.Event() for _ in range(5)
        )
        received, cues = [], []

        class Audio(FakeAudio):
            def set_input(self, enabled):
                super().set_input(enabled)
                self.enabled = enabled
                if enabled:
                    receiving.set()

            async def cue(self, _frequency, *, duration=0.12):
                cues.append((_frequency, duration))
                cue_started.set()
                await cue_allowed.wait()

        audio = Audio()

        async def server(socket):
            connected.set()
            await ready_allowed.wait()
            await socket.send('{"type":"ready"}')
            received.append(await socket.recv())
            await socket.send('{"type":"final","text":"hello paste that"}')
            async for _frame in socket:
                pass

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            pending = asyncio.create_task(capture(local, audio, Capture(), lambda _: None))
            await asyncio.wait_for(connected.wait(), 1)
            audio.queue.put_nowait(bytes([1, 0]) * 3840)
            await asyncio.wait_for(cue_started.wait(), 1)
            audio.queue.put_nowait(bytes([2, 0]) * 3840)
            cue_allowed.set()
            await asyncio.wait_for(receiving.wait(), 1)
            audio.queue.get_nowait()
            audio.queue.put_nowait(bytes([3, 0]) * 3840)
            ready_allowed.set()
            assert await asyncio.wait_for(pending, 1) == Request(Ending.PASTE, "hello")
        assert received == [bytes([3, 0]) * 1920]
        assert cues == [(440, 0.04), (520, 0.08), (660, 0.12)]

    asyncio.run(behavior())


@pytest.mark.parametrize("failure", ["error", "cancel"])
def test_start_cue_failure_cancels_connection_and_restores_input(config, failure):
    async def behavior():
        cue_started = asyncio.Event()
        server_closed = asyncio.Event()

        class Audio(FakeAudio):
            enabled = True

            def set_input(self, enabled):
                self.enabled = enabled

            async def cue(self, _frequency, *, duration=0.12):
                cue_started.set()
                if failure == "error":
                    raise RuntimeError("cue failed")
                await asyncio.Event().wait()

        audio = Audio()

        async def server(socket):
            await socket.wait_closed()
            server_closed.set()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            pending = asyncio.create_task(capture(local, audio, Capture(), lambda _: None))
            await asyncio.wait_for(cue_started.wait(), 1)
            if failure == "cancel":
                pending.cancel()
            with pytest.raises(RuntimeError if failure == "error" else asyncio.CancelledError):
                await pending
            assert audio.enabled

    asyncio.run(behavior())


def test_confirmation_waits_for_ready_then_cue_and_discards_prior_audio(config):
    async def behavior():
        connected, allow_ready, cue_started, allow_cue, prompted = (
            asyncio.Event() for _ in range(5)
        )
        received = []

        class Audio(FakeAudio):
            def set_input(self, enabled):
                while not self.queue.empty():
                    self.queue.get_nowait()
                self.enabled = enabled
                if enabled:
                    prompted.set()

            async def cue(self, _frequency, *, duration=0.12):
                assert duration == 0.12
                cue_started.set()
                await allow_cue.wait()

        audio = Audio()

        async def server(socket):
            connected.set()
            await allow_ready.wait()
            await socket.send('{"type":"ready"}')
            received.append(await socket.recv())
            await socket.send('{"type":"final","text":"yes"}')

        from thalvi_desktop.capture import Confirmation

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            pending = asyncio.create_task(
                capture(local, audio, Confirmation(), lambda _: None, confirmation=True)
            )
            await asyncio.wait_for(connected.wait(), 1)
            assert not cue_started.is_set() and not audio.enabled
            audio.queue.put_nowait(bytes([1, 0]) * 3840)
            allow_ready.set()
            await asyncio.wait_for(cue_started.wait(), 1)
            audio.queue.put_nowait(bytes([2, 0]) * 3840)
            allow_cue.set()
            await asyncio.wait_for(prompted.wait(), 1)
            audio.queue.put_nowait(bytes([3, 0]) * 3840)
            assert await asyncio.wait_for(pending, 1) == Request(Ending.SUBMIT, "")
        assert received == [bytes([3, 0]) * 1920]

    asyncio.run(behavior())


def test_connection_failure_cancels_cue_and_records_outcomes(monkeypatch, config, tmp_path):
    from thalvi_desktop import client
    from thalvi_desktop.metrics import Metrics

    async def behavior():
        cue_started, cue_cancelled = asyncio.Event(), asyncio.Event()

        class Audio(FakeAudio):
            enabled = True

            def set_input(self, enabled):
                self.enabled = enabled

            async def cue(self, _frequency, *, duration=0.12):
                cue_started.set()
                try:
                    await asyncio.Event().wait()
                finally:
                    cue_cancelled.set()

        async def connect_failure(*_args, **_kwargs):
            await cue_started.wait()
            raise OSError("connection failed")

        monkeypatch.setattr(client, "connect", connect_failure)
        audio = Audio()
        metrics = Metrics(tmp_path / "metrics.jsonl")
        with pytest.raises(OSError, match="connection failed"):
            await capture(config, audio, Capture(), lambda _: None, metrics=metrics)
        assert audio.enabled and cue_cancelled.is_set()
        await metrics.flush()
        rows = [json.loads(line) for line in metrics.path.read_text().splitlines()]
        assert {(row["event"], row["outcome"]) for row in rows} == {
            ("desktop_transcription_ready", "error"),
            ("desktop_start_cue", "cancelled"),
        }

    asyncio.run(behavior())


@pytest.mark.parametrize("cancelled", [False, True])
def test_end_cue_precedes_cleanup_and_delivery_and_can_be_cancelled(config, cancelled):
    from thalvi_desktop.workflow import PreviewHost, Workflow

    async def behavior():
        cues = []
        started = asyncio.Event()
        release = asyncio.Event()
        closed = asyncio.Event()
        host = PreviewHost(lambda _: None)

        class Audio(FakeAudio):
            def set_input(self, enabled):
                self.enabled = enabled
                super().set_input(enabled)

            async def cue(self, frequency, *, duration=0.12):
                cues.append(frequency)
                if frequency == 660:
                    assert not self.enabled
                    started.set()
                    await release.wait()

        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            for kind, text in [
                ("partial", "paste that"),
                ("final", "wrong scratch that"),
                ("final", "near paste that please"),
                ("final", "hello paste"),
                ("final", "that"),
            ]:
                await socket.send(json.dumps({"type": kind, "text": text}))
            await socket.wait_closed()
            closed.set()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")

            async def no_confirmation():
                raise AssertionError("Paste does not need confirmation.")

            async def flow():
                request = await capture(local, Audio(), Capture(), lambda _: None)
                assert await Workflow(local, host, lambda _: None).finish(
                    request, host.capture(), no_confirmation
                )

            pending = asyncio.create_task(flow())
            try:
                await asyncio.wait_for(started.wait(), 1)
                assert cues == [440, 520, 660]
                assert not closed.is_set() and not pending.done()
                assert host.operations == []
                if cancelled:
                    pending.cancel()
                    with pytest.raises(asyncio.CancelledError):
                        await asyncio.wait_for(pending, 1)
                else:
                    release.set()
                    await asyncio.wait_for(pending, 1)
            finally:
                release.set()
                pending.cancel()
                await asyncio.gather(pending, return_exceptions=True)
        assert cues == [440, 520, 660]
        assert host.operations == (
            [] if cancelled else [("deliver", "near paste that please hello", False)]
        )

    asyncio.run(behavior())


@pytest.mark.parametrize("confirmation,ending", [(False, "hello paste that"), (True, "yes")])
def test_end_cue_failure_and_confirmation(config, confirmation, ending):
    from thalvi_desktop.capture import Confirmation
    from thalvi_desktop.workflow import PreviewHost, Workflow

    async def behavior():
        cues = []
        notices = []

        class Audio(FakeAudio):
            async def cue(self, frequency, *, duration=0.12):
                cues.append(frequency)
                if frequency == 660:
                    raise RuntimeError("speaker failed")

        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            await socket.send(json.dumps({"type": "final", "text": ending}))
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            result = await asyncio.wait_for(
                capture(
                    local,
                    Audio(),
                    Confirmation() if confirmation else Capture(),
                    notices.append,
                    confirmation=confirmation,
                ),
                1,
            )
        assert result == Request(
            Ending.SUBMIT if confirmation else Ending.PASTE, "" if confirmation else "hello"
        )
        assert cues == ([880] if confirmation else [440, 520, 660])
        assert confirmation or any("speaker failed" in notice for notice in notices)
        if not confirmation:
            host = PreviewHost(lambda _: None)

            async def no_confirmation():
                raise AssertionError("Paste does not need confirmation.")

            assert await Workflow(config, host, lambda _: None).finish(
                result, host.capture(), no_confirmation
            )
            assert host.operations == [("deliver", "hello", False)]

    asyncio.run(behavior())


def _cue_harness(monkeypatch, failing=()):
    from thalvi_desktop import client
    from thalvi_desktop.audio import Audio
    from thalvi_desktop.pending import Pending

    class Voice:
        def speech(self, frame):
            return frame[0] == 1

    monkeypatch.setattr(client, "VoiceActivity", Voice)
    monkeypatch.setattr(client, "Pending", Pending)

    class Socket:
        def __init__(self):
            self.events = asyncio.Queue()
            self.sent = asyncio.Queue()

        def __aiter__(self):
            return self

        async def __anext__(self):
            return await self.events.get()

        async def send(self, _frame):
            self.sent.put_nowait(None)

        def offer(self, event):
            self.events.put_nowait(json.dumps(event))

    class TestAudio(Audio):
        def __init__(self):
            super().__init__()
            self.cues = []
            self.holds = {}

        async def cue(self, frequency, *, duration=0.12):
            self.cues.append(frequency)
            if frequency in failing:
                raise RuntimeError("speaker failed")
            hold = self.holds.get(frequency)
            if hold is not None:
                await hold.wait()

    return Socket, TestAudio


def test_ending_cues_mark_pending_cancelled_and_accepted(monkeypatch):
    from thalvi_desktop import client

    socket_type, audio_type = _cue_harness(monkeypatch)

    async def behavior():
        socket, audio = socket_type(), audio_type()
        task = asyncio.create_task(
            client._capture_events(socket, audio, Capture(), lambda _: None, pause=0.3)
        )

        async def feed():
            while True:
                if audio.frames.empty():
                    audio.frames.put_nowait(bytes(7680))
                await asyncio.sleep(0.01)

        feeder = asyncio.create_task(feed())

        async def until_cues(expected):
            async with asyncio.timeout(2):
                while audio.cues != expected:
                    await asyncio.sleep(0)

        audio.holds[520] = asyncio.Event()
        try:
            audio.frames.put_nowait(bytes(7680))
            await asyncio.wait_for(socket.sent.get(), 1)
            socket.offer({"type": "final", "text": "hello submit"})
            await until_cues([520])
            while not socket.sent.empty():
                socket.sent.get_nowait()
            audio.frames.put_nowait(bytes([1]) + bytes(7679))
            await asyncio.sleep(0.05)
            assert socket.sent.qsize() == 0, "audio heard during a cue was forwarded"
            audio.holds[520].set()
            async with asyncio.timeout(2):
                while not audio.input_enabled:
                    await asyncio.sleep(0)
            audio.frames.put_nowait(bytes([1]) + bytes(7679))
            await until_cues([520, 330])
            socket.offer({"type": "final", "text": "submit"})
            await until_cues([520, 330, 520])
            result = await asyncio.wait_for(task, 2)
        finally:
            feeder.cancel()
            task.cancel()
            await asyncio.gather(feeder, task, return_exceptions=True)
        assert result == Request(Ending.SUBMIT, "hello submit", pause_confirmed=True)
        assert audio.cues == [520, 330, 520, 660]

    asyncio.run(behavior())


def test_pending_cue_failure_is_explained_and_capture_continues(monkeypatch):
    from thalvi_desktop import client

    socket_type, audio_type = _cue_harness(monkeypatch, failing={520})

    async def behavior():
        socket, audio = socket_type(), audio_type()
        notices = []
        task = asyncio.create_task(
            client._capture_events(socket, audio, Capture(), notices.append, pause=0.3)
        )

        async def feed():
            while True:
                if audio.frames.empty():
                    audio.frames.put_nowait(bytes(7680))
                await asyncio.sleep(0.01)

        feeder = asyncio.create_task(feed())
        try:
            audio.frames.put_nowait(bytes(7680))
            await asyncio.wait_for(socket.sent.get(), 1)
            socket.offer({"type": "final", "text": "hello submit"})
            result = await asyncio.wait_for(task, 2)
        finally:
            feeder.cancel()
            task.cancel()
            await asyncio.gather(feeder, task, return_exceptions=True)
        assert result == Request(Ending.SUBMIT, "hello", pause_confirmed=True)
        assert audio.cues == [520, 660]
        assert any("speaker failed" in notice for notice in notices)

    asyncio.run(behavior())


def _cue_behavior(monkeypatch, failing=()):
    from thalvi_desktop import client

    socket_type, audio_type = _cue_harness(monkeypatch, failing)
    socket, audio = socket_type(), audio_type()

    async def feed():
        while True:
            if audio.frames.empty():
                audio.frames.put_nowait(bytes(7680))
            await asyncio.sleep(0.01)

    async def until_cues(expected):
        async with asyncio.timeout(2):
            while audio.cues != expected:
                await asyncio.sleep(0)

    return client, socket, audio, feed, until_cues


def test_replaced_ending_plays_cancelled_then_pending_cue(monkeypatch):
    client, socket, audio, feed, until_cues = _cue_behavior(monkeypatch)

    async def behavior():
        task = asyncio.create_task(
            client._capture_events(socket, audio, Capture(), lambda _: None, pause=0.3)
        )
        feeder = asyncio.create_task(feed())
        try:
            audio.frames.put_nowait(bytes(7680))
            await asyncio.wait_for(socket.sent.get(), 1)
            socket.offer({"type": "final", "text": "hello paste that"})
            await until_cues([520])
            socket.offer({"type": "final", "text": "actually submit"})
            await until_cues([520, 330, 520])
            result = await asyncio.wait_for(task, 2)
        finally:
            feeder.cancel()
            task.cancel()
            await asyncio.gather(feeder, task, return_exceptions=True)
        assert result == Request(Ending.SUBMIT, "hello paste that actually", pause_confirmed=True)
        assert audio.cues == [520, 330, 520, 660]

    asyncio.run(behavior())


def test_speech_during_cancel_cue_is_not_forwarded(monkeypatch):
    client, socket, audio, feed, until_cues = _cue_behavior(monkeypatch)

    async def behavior():
        task = asyncio.create_task(
            client._capture_events(socket, audio, Capture(), lambda _: None, pause=0.3)
        )
        feeder = asyncio.create_task(feed())
        audio.holds[330] = asyncio.Event()
        try:
            audio.frames.put_nowait(bytes(7680))
            await asyncio.wait_for(socket.sent.get(), 1)
            socket.offer({"type": "final", "text": "hello submit"})
            await until_cues([520])
            while not socket.sent.empty():
                socket.sent.get_nowait()
            audio.frames.put_nowait(bytes([1]) + bytes(7679))
            await until_cues([520, 330])
            audio.frames.put_nowait(bytes([1]) + bytes(7679))
            await asyncio.sleep(0.05)
            assert socket.sent.qsize() == 0, "audio heard during a cue was forwarded"
            audio.holds[330].set()
            socket.offer({"type": "final", "text": "submit"})
            await until_cues([520, 330, 520])
            result = await asyncio.wait_for(task, 2)
        finally:
            feeder.cancel()
            task.cancel()
            await asyncio.gather(feeder, task, return_exceptions=True)
        assert result == Request(Ending.SUBMIT, "hello submit", pause_confirmed=True)
        assert audio.cues == [520, 330, 520, 660]

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "continuation", ["voice", "queued_voice", "partial", "speech_start", "final", "error"]
)
@pytest.mark.parametrize("metrics_on", [False, True])
def test_pending_deadline_gives_fresh_audio_and_recognizer_events_priority(
    monkeypatch, continuation, metrics_on
):
    from thalvi_desktop import client
    from thalvi_desktop.audio import Audio

    class Voice:
        def speech(self, frame):
            return frame[0] == 1

    monkeypatch.setattr(client, "VoiceActivity", Voice)

    class SlowMetrics:
        async def record(self, _event, _started, _outcome, *, buffered=False):
            if not buffered:
                await asyncio.Future()

    class Socket:
        def __init__(self):
            self.events = asyncio.Queue()
            self.sent = asyncio.Event()

        def __aiter__(self):
            return self

        async def __anext__(self):
            return await self.events.get()

        async def send(self, _frame):
            self.sent.set()

        def offer(self, event):
            self.events.put_nowait(json.dumps(event))

    class TestAudio(Audio):
        def __init__(self):
            super().__init__()
            self.cues = []

        async def cue(self, frequency, *, duration=0.12):
            self.cues.append(frequency)

    async def behavior():
        socket, audio = Socket(), TestAudio()
        pending, cleared = asyncio.Event(), asyncio.Event()

        def on_pending(request, _seconds):
            (pending if request else cleared).set()

        task = asyncio.create_task(
            client._capture_events(
                socket,
                audio,
                Capture(),
                lambda _message: None,
                metrics=SlowMetrics() if metrics_on else None,
                pending_notice=on_pending,
            )
        )
        try:
            audio.frames.put_nowait(bytes(7680))
            await asyncio.wait_for(socket.sent.wait(), 1)
            socket.offer({"type": "final", "text": "hello submit"})
            await asyncio.wait_for(pending.wait(), 1)
            loop = asyncio.get_running_loop()
            if continuation in {"voice", "queued_voice"}:
                if continuation == "queued_voice":
                    loop.call_soon(audio.frames.put_nowait, bytes(7680))
                loop.call_soon(audio.frames.put_nowait, bytes([1]) + bytes(7679))
            else:
                event = {"type": continuation}
                if continuation in {"partial", "final"}:
                    event["text"] = "more words"
                elif continuation == "error":
                    event["message"] = "recognizer lost"
                loop.call_soon(socket.offer, event)
                loop.call_soon(audio.frames.put_nowait, bytes(7680))
            if continuation == "error":
                with pytest.raises(RuntimeError, match="recognizer lost"):
                    await asyncio.wait_for(task, 1)
            else:
                await asyncio.wait_for(cleared.wait(), 1)
                assert not task.done()
            assert 660 not in audio.cues
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())


@pytest.mark.parametrize("backend", ["fake", "webrtc"])
def test_vad_tail_waits_for_full_quiet_and_accepts(monkeypatch, backend):
    import math
    import struct

    from thalvi_desktop import client
    from thalvi_desktop.audio import Audio
    from thalvi_desktop.pending import Pending
    from thalvi_desktop.vad import VoiceActivity

    class TailVoice:
        def __init__(self):
            self.frames = 0

        def speech(self, _frame):
            self.frames += 1
            return self.frames <= 8

    if backend == "webrtc":
        pytest.importorskip("webrtcvad")
    monkeypatch.setattr(
        client, "VoiceActivity", VoiceActivity if backend == "webrtc" else TailVoice
    )
    monkeypatch.setattr(client, "Pending", Pending)

    class Socket:
        def __init__(self):
            self.events = asyncio.Queue()
            self.sent = asyncio.Queue()

        def __aiter__(self):
            return self

        async def __anext__(self):
            return await self.events.get()

        async def send(self, _frame):
            self.sent.put_nowait(None)

    class TestAudio(Audio):
        async def cue(self, _frequency, *, duration=0.12):
            pass

    async def behavior():
        socket, audio = Socket(), TestAudio()
        pending = asyncio.Event()
        records = []

        class Metrics:
            async def record(self, event, _started, outcome, **_kwargs):
                records.append((event, outcome))

        clock = [100.0]
        loop = asyncio.get_running_loop()
        monkeypatch.setattr(loop, "time", lambda: clock[0])
        task = asyncio.create_task(
            client._capture_events(
                socket,
                audio,
                Capture(),
                lambda _: None,
                metrics=Metrics(),
                pending_notice=lambda request, _: pending.set() if request else None,
            )
        )

        async def frame(data):
            if task.done():
                return
            audio.frames.put_nowait(data)
            sent = asyncio.create_task(socket.sent.get())
            try:
                await asyncio.wait({task, sent}, return_when=asyncio.FIRST_COMPLETED)
            finally:
                sent.cancel()
                await asyncio.gather(sent, return_exceptions=True)

        try:
            speech = struct.pack(
                "<3840h",
                *(int(12000 * math.sin(2 * math.pi * 180 * i / 48000)) for i in range(3840)),
            )
            for _ in range(6):
                await frame(speech)
            socket.events.put_nowait(json.dumps({"type": "final", "text": "hello paste that"}))
            await pending.wait()
            for _ in range(3):
                await frame(bytes(7680))
            for _ in range(10):
                await asyncio.sleep(0)
            clock[0] += 2.49
            await frame(bytes(7680))
            assert not task.done()
            for _ in range(10):
                await asyncio.sleep(0)
            clock[0] += 0.02
            for _ in range(20):
                await frame(bytes(7680))
                for _ in range(10):
                    await asyncio.sleep(0)
                if task.done():
                    break
            assert task.done()
            assert await task == Request(Ending.PASTE, "hello")
            assert [outcome for event, outcome in records if event == "desktop_ending_state"] == [
                "waiting_for_quiet",
                "quiet_started",
                "accepted",
            ]
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())


@pytest.mark.parametrize("partial", ["", "hello paste that", "new words"])
def test_partial_during_original_speech_only_cancels_when_nonempty(monkeypatch, partial):
    from thalvi_desktop import client
    from thalvi_desktop.audio import Audio
    from thalvi_desktop.pending import Pending

    class Voice:
        def speech(self, _frame):
            return True

    class Socket:
        def __init__(self):
            self.events = asyncio.Queue()
            self.sent = asyncio.Event()

        def __aiter__(self):
            return self

        async def __anext__(self):
            return await self.events.get()

        async def send(self, _frame):
            self.sent.set()

    monkeypatch.setattr(client, "VoiceActivity", Voice)
    monkeypatch.setattr(client, "Pending", Pending)

    class TestAudio(Audio):
        async def cue(self, _frequency, *, duration=0.12):
            pass

    async def behavior():
        socket, audio = Socket(), TestAudio()
        armed, cleared, heard = asyncio.Event(), asyncio.Event(), asyncio.Event()

        def notice(message):
            if message.startswith("Hearing"):
                heard.set()

        def pending_notice(request, _seconds):
            (armed if request else cleared).set()

        task = asyncio.create_task(
            client._capture_events(socket, audio, Capture(), notice, pending_notice=pending_notice)
        )
        try:
            audio.frames.put_nowait(bytes(7680))
            await socket.sent.wait()
            socket.events.put_nowait(json.dumps({"type": "final", "text": "hello paste that"}))
            await armed.wait()
            socket.events.put_nowait(json.dumps({"type": "partial", "text": partial}))
            await heard.wait()
            assert cleared.is_set() == bool(partial)
            assert not task.done()
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())
