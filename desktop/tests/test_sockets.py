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
from thalvi_desktop.cues import Cue, Cues


class FakeAudio:
    def __init__(self):
        self.queue = asyncio.Queue()
        self.queue.put_nowait(bytes(7680))
        self.played = []
        self.cues = []
        self.stopped = 0
        self.sent_bytes = self.played_bytes = 0

    def set_input(self, enabled):
        while not self.queue.empty():
            self.queue.get_nowait()
        if enabled:
            self.queue.put_nowait(bytes(7680))

    async def play_cue(self, cue):
        self.cues.append(cue)

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
                while not self.queue.empty():
                    self.queue.get_nowait()
                self.enabled = enabled
                if enabled:
                    receiving.set()

            async def play_cue(self, cue):
                cues.append(cue)
                if cue == Cue.WAKE:
                    cue_started.set()
                    await cue_allowed.wait()

        audio = Audio()

        async def server(socket):
            connected.set()
            await ready_allowed.wait()
            await socket.send('{"type":"ready"}')
            received.append(await socket.recv())
            await socket.send('{"type":"final","text":"hello paste that"}')
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            pending = asyncio.create_task(capture(local, audio, Capture(), lambda _: None))
            await asyncio.wait_for(connected.wait(), 1)
            audio.queue.put_nowait(bytes([1, 0]) * 3840)
            await asyncio.wait_for(cue_started.wait(), 1)
            audio.queue.put_nowait(bytes([2, 0]) * 3840)
            cue_allowed.set()
            await asyncio.wait_for(receiving.wait(), 1)
            audio.queue.put_nowait(bytes([3, 0]) * 3840)
            ready_allowed.set()
            assert await asyncio.wait_for(pending, 1) == Request(Ending.PASTE, "hello")
        assert received == [bytes([3, 0]) * 1920]
        assert cues == [Cue.WAKE, Cue.PROVISIONAL, Cue.ACCEPTED]

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

            async def play_cue(self, cue):
                assert cue == Cue.WAKE
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

            async def play_cue(self, cue):
                assert cue == Cue.WAKE
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

            async def play_cue(self, cue):
                assert cue == Cue.WAKE
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

            async def play_cue(self, cue):
                cues.append(cue)
                if cue == Cue.ACCEPTED:
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
                assert cues == [Cue.WAKE, Cue.PROVISIONAL, Cue.ACCEPTED]
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
        assert cues == [Cue.WAKE, Cue.PROVISIONAL, Cue.ACCEPTED]
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
            async def play_cue(self, cue):
                cues.append(cue)
                if cue == Cue.ACCEPTED:
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
        assert cues == ([Cue.WAKE] if confirmation else [Cue.WAKE, Cue.PROVISIONAL, Cue.ACCEPTED])
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

        async def play_cue(self, cue):
            self.cues.append(cue)

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
                async with asyncio.timeout(1):
                    while audio.cues[-1] != Cue.CANCELLED:
                        await asyncio.sleep(0)
            expected = [Cue.PROVISIONAL] + ([] if continuation == "error" else [Cue.CANCELLED])
            assert audio.cues == expected
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
        async def play_cue(self, _cue):
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
        async def play_cue(self, _cue):
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
            await asyncio.wait_for(socket.sent.wait(), 1)
            socket.events.put_nowait(json.dumps({"type": "final", "text": "hello paste that"}))
            await asyncio.wait_for(armed.wait(), 1)
            socket.events.put_nowait(json.dumps({"type": "partial", "text": partial}))
            await asyncio.wait_for(heard.wait(), 1)
            assert cleared.is_set() == bool(partial)
            assert not task.done()
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())


@pytest.mark.parametrize("disabled", [None, Cue.PROVISIONAL, Cue.CANCELLED])
def test_provisional_ending_then_continued_speech_plays_provisional_then_cancelled(
    monkeypatch, disabled
):
    from thalvi_desktop import client
    from thalvi_desktop.pending import Pending

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

    monkeypatch.setattr(client, "Pending", Pending)

    async def behavior():
        socket, audio = Socket(), FakeAudio()
        heard = asyncio.Event()
        cues = Cues(**{disabled.value: False}) if disabled else Cues()
        task = asyncio.create_task(
            client._capture_events(
                socket,
                audio,
                Capture(),
                lambda message: heard.set() if message.startswith("Hearing") else None,
                cues=cues,
            )
        )
        try:
            await asyncio.wait_for(socket.sent.wait(), 1)
            socket.events.put_nowait(json.dumps({"type": "final", "text": "hello paste that"}))
            socket.events.put_nowait(json.dumps({"type": "partial", "text": "more words"}))
            await asyncio.wait_for(heard.wait(), 1)
            assert audio.cues == [c for c in (Cue.PROVISIONAL, Cue.CANCELLED) if c != disabled]
            assert not task.done()
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())


@pytest.mark.parametrize("disabled", [None, Cue.WAKE, Cue.PROVISIONAL, Cue.ACCEPTED])
def test_disabled_cues_are_skipped_and_played_cues_are_recorded(config, tmp_path, disabled):
    from thalvi_desktop.metrics import Metrics

    async def behavior():
        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            await socket.send('{"type":"final","text":"hello paste that"}')
            await socket.wait_closed()

        audio, metrics = FakeAudio(), Metrics(tmp_path / "metrics.jsonl")
        cues = Cues(**{disabled.value: False}) if disabled else Cues()
        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(
                config,
                server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}",
                cues=cues,
            )
            await asyncio.wait_for(
                capture(local, audio, Capture(), lambda _: None, metrics=metrics), 2
            )
        assert audio.cues == [c for c in (Cue.WAKE, Cue.PROVISIONAL, Cue.ACCEPTED) if c != disabled]
        await metrics.flush()
        rows = [json.loads(line) for line in metrics.path.read_text().splitlines()]
        assert [r["outcome"] for r in rows if r["event"] == "desktop_cue"] == (
            [] if disabled == Cue.PROVISIONAL else ["provisional"]
        )

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "cues, speech_fails, expected",
    [
        (Cues(), False, [Cue.WAKE]),
        (Cues(wake=False), False, []),
        (Cues(), True, [Cue.WAKE]),
        (Cues(error=False), True, [Cue.WAKE]),
    ],
)
def test_conversation_plays_wake_cue_and_raises_on_speech_failure(
    config, cues, speech_fails, expected
):
    class Audio(FakeAudio):
        def play(self, pcm):
            raise RuntimeError("Speech playback could not open.")

    async def behavior():
        audio = Audio()

        async def server(socket):
            await socket.recv()
            await socket.send('{"type":"state","state":"listening"}')
            if speech_fails:
                await socket.send(b"\0\0")
            else:
                await socket.send('{"type":"user","text":"never mind"}')
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(
                config,
                server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}",
                cues=cues,
            )
            if speech_fails:
                with pytest.raises(RuntimeError, match="could not open"):
                    await asyncio.wait_for(conversation(local, audio, lambda _: None), 2)
            else:
                await asyncio.wait_for(conversation(local, audio, lambda _: None), 2)
        assert audio.cues == expected

    asyncio.run(behavior())


def test_cancel_ending_plays_the_cancelled_cue_not_accepted(config):
    async def behavior():
        audio = FakeAudio()

        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            await socket.send('{"type":"final","text":"never mind"}')
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            result = await asyncio.wait_for(capture(local, audio, Capture(), lambda _: None), 2)
        assert result == Request(Ending.CANCEL, "")
        assert audio.cues == [Cue.WAKE, Cue.CANCELLED]

    asyncio.run(behavior())


@pytest.mark.parametrize("overlap", [False, True])
def test_cues_suspend_frame_watchdog_and_rearm_it_after_playback(monkeypatch, overlap):
    from thalvi_desktop import client
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

    class Audio(FakeAudio):
        def __init__(self):
            super().__init__()
            self.requested = asyncio.Queue()
            self.started = asyncio.Queue()
            self.releases = [asyncio.Event() for _ in range(3)]
            self.lock = asyncio.Lock()

        async def play_cue(self, cue):
            index = len(self.cues)
            self.cues.append(cue)
            self.requested.put_nowait(cue)
            async with self.lock:
                self.started.put_nowait(cue)
                await self.releases[index].wait()

    async def behavior():
        socket, audio = Socket(), Audio()
        clock = [100.0]
        loop = asyncio.get_running_loop()
        monkeypatch.setattr(loop, "time", lambda: clock[0])
        if not overlap:
            audio.queue.get_nowait()
            audio.queue.put_nowait(bytes([1, 0]) * 3840)
        task = asyncio.create_task(client._capture_events(socket, audio, Capture(), lambda _: None))

        async def advance(seconds):
            clock[0] += seconds
            for _ in range(10):
                await asyncio.sleep(0)

        try:
            await socket.sent.get()
            socket.events.put_nowait(json.dumps({"type": "final", "text": "hello paste that"}))
            assert await audio.started.get() == Cue.PROVISIONAL
            await audio.requested.get()
            if overlap:
                audio.releases[0].set()
                audio.queue.put_nowait(bytes([1, 0]) * 3840)
                assert await audio.started.get() == Cue.CANCELLED
                await audio.requested.get()
                socket.events.put_nowait(json.dumps({"type": "final", "text": "more words submit"}))
                assert await audio.requested.get() == Cue.PROVISIONAL
                audio.releases[1].set()
                assert await audio.started.get() == Cue.PROVISIONAL
                await socket.sent.get()
            await advance(0.4)
            assert not task.done()
            audio.releases[2 if overlap else 0].set()
            await advance(0)
            await advance(0.24)
            assert not task.done()
            await advance(0.02)
            assert task.done()
            with pytest.raises(RuntimeError, match="Microphone audio stopped"):
                await task
            assert audio.cues == (
                [Cue.PROVISIONAL, Cue.CANCELLED, Cue.PROVISIONAL] if overlap else [Cue.PROVISIONAL]
            )
        finally:
            for release in audio.releases:
                release.set()
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(behavior())


def test_secondary_local_endpoint_receives_wav_and_replaces_accepted_text(config, tmp_path):
    import email
    import io
    import threading
    import wave
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    from thalvi_desktop.config import SecondarySTT
    from thalvi_desktop.metrics import Metrics

    received = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            message = email.message_from_bytes(
                (
                    f"Content-Type: {self.headers['Content-Type']}\r\nMIME-Version: 1.0\r\n\r\n"
                ).encode()
                + body
            )
            parts = {
                part.get_param("name", header="content-disposition"): part
                for part in message.get_payload()
            }
            received.append((self.path, self.headers.get("Authorization"), parts))
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"text":"More accurate words paste that"}')

        def log_message(self, *_args):
            pass

    http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=http.serve_forever)
    thread.start()
    try:

        async def behavior():
            sent = []

            async def server(socket):
                await socket.send('{"type":"ready"}')
                sent.append(await socket.recv())
                await socket.send('{"type":"final","text":"live words paste that"}')
                async for frame in socket:
                    sent.append(frame)

            async with serve(server, "127.0.0.1", 0) as listener:
                local = replace(
                    config,
                    server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}",
                    secondary_stt=SecondarySTT(
                        f"http://127.0.0.1:{http.server_port}/v1", "model", "secret"
                    ),
                )
                metrics = Metrics(tmp_path / "metrics.jsonl")
                result = await asyncio.wait_for(
                    capture(local, FakeAudio(), Capture(), lambda _: None, metrics=metrics), 3
                )
                await metrics.flush()
            assert result == Request(Ending.PASTE, "More accurate words")
            path, auth, parts = received[0]
            assert (path, auth) == ("/v1/audio/transcriptions", "Bearer secret")
            assert parts["model"].get_payload(decode=True) == b"model"
            assert parts["file"].get_filename() == "capture.wav"
            assert parts["file"].get_content_type() == "audio/wav"
            with wave.open(io.BytesIO(parts["file"].get_payload(decode=True))) as wav:
                assert (wav.getnchannels(), wav.getframerate(), wav.getsampwidth()) == (1, 24000, 2)
                assert wav.readframes(wav.getnframes()) == b"".join(sent)
            assert [
                (r["event"], r["outcome"])
                for r in map(json.loads, metrics.path.read_text().splitlines())
                if r["event"] == "desktop_secondary_stt"
            ] == [("desktop_secondary_stt", "ok")]

        asyncio.run(behavior())
    finally:
        http.shutdown()
        thread.join()
        http.server_close()


def test_secondary_truncated_response_uses_live_transcript():
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    from thalvi_desktop import client
    from thalvi_desktop.config import SecondarySTT

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers["Content-Length"]))
            self.send_response(200)
            self.send_header("Content-Length", "100")
            self.end_headers()
            self.wfile.write(b'{"text":')

        def log_message(self, *_args):
            pass

    http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=http.serve_forever)
    thread.start()
    try:
        notices = []
        original = Request(Ending.PASTE, "live words")
        result = asyncio.run(
            client.secondary_transcription(
                SecondarySTT(f"http://127.0.0.1:{http.server_port}/v1", "model"),
                b"",
                original,
                {},
                notices.append,
                None,
            )
        )
        assert result == original
        assert any("Using the live transcript" in notice for notice in notices)
    finally:
        http.shutdown()
        thread.join()
        http.server_close()


@pytest.mark.parametrize(
    "failure", ["network", "malformed", "timeout", "different", "scratch", "empty"]
)
def test_secondary_failure_uses_live_transcript(config, monkeypatch, failure):
    from thalvi_desktop import client
    from thalvi_desktop.config import SecondarySTT

    def secondary(*_args):
        if failure == "network":
            raise OSError("network")
        if failure == "malformed":
            raise ValueError("malformed")
        if failure == "timeout":
            raise TimeoutError()
        if failure == "empty":
            return ""
        return "different scratch that" if failure == "scratch" else "different submit"

    monkeypatch.setattr(client, "_secondary_request", secondary)
    notices = []
    original = Request(Ending.PASTE, "live words", True)
    result = asyncio.run(
        client.secondary_transcription(
            SecondarySTT("http://localhost", "model"),
            b"",
            original,
            config.endings,
            notices.append,
            None,
        )
    )
    assert result == original
    assert any("Using the live transcript" in notice for notice in notices)


def test_secondary_cancellation_does_not_return_request(config, monkeypatch):
    import threading

    from thalvi_desktop import client
    from thalvi_desktop.config import SecondarySTT

    started, release = threading.Event(), threading.Event()

    def secondary(*_args):
        started.set()
        release.wait(2)
        return "words paste that"

    monkeypatch.setattr(client, "_secondary_request", secondary)

    async def behavior():
        task = asyncio.create_task(
            client.secondary_transcription(
                SecondarySTT("http://localhost", "model"),
                b"",
                Request(Ending.PASTE, "live"),
                config.endings,
                lambda _: None,
                None,
            )
        )
        assert await asyncio.to_thread(started.wait, 1)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        release.set()

    asyncio.run(behavior())


@pytest.mark.parametrize("confirmation,ending", [(False, "never mind"), (True, "yes")])
def test_secondary_not_called_for_cancel_or_confirmation(config, monkeypatch, confirmation, ending):
    from thalvi_desktop import client
    from thalvi_desktop.capture import Confirmation
    from thalvi_desktop.config import SecondarySTT

    def unexpected(*_args):
        pytest.fail("Secondary endpoint must not be called")

    monkeypatch.setattr(client, "_secondary_request", unexpected)

    async def behavior():
        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            await socket.send(json.dumps({"type": "final", "text": ending}))
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(
                config,
                server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}",
                secondary_stt=SecondarySTT("http://localhost", "model"),
            )
            result = await asyncio.wait_for(
                capture(
                    local,
                    FakeAudio(),
                    Confirmation() if confirmation else Capture(),
                    lambda _: None,
                    confirmation=confirmation,
                ),
                2,
            )
        assert result.ending == (Ending.SUBMIT if confirmation else Ending.CANCEL)

    asyncio.run(behavior())


def test_secondary_not_called_after_scratch(config, monkeypatch):
    from thalvi_desktop import client
    from thalvi_desktop.config import SecondarySTT

    def unexpected(*_args):
        pytest.fail("A scratched capture must keep the live transcript")

    monkeypatch.setattr(client, "_secondary_request", unexpected)

    async def behavior():
        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.recv()
            for text in [
                "Keep first.",
                "Throw this away scratch that.",
                "Goodbye paste that.",
            ]:
                await socket.send(json.dumps({"type": "final", "text": text}))
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(
                config,
                server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}",
                secondary_stt=SecondarySTT("http://localhost", "model"),
            )
            result = await asyncio.wait_for(
                capture(local, FakeAudio(), Capture(), lambda _: None), 2
            )
        assert result == Request(Ending.PASTE, "Keep first. Goodbye")

    asyncio.run(behavior())


def test_secondary_config_validation_and_default(tmp_path):
    from thalvi_desktop.config import SecondarySTT

    path = tmp_path / "config.json"

    def load(value):
        path.write_text(json.dumps(value))
        return Config.load(path)

    assert load({}).secondary_stt is None
    assert load({"stt": {}}).secondary_stt is None
    assert load(
        {"stt": {"secondary": {"url": "http://localhost/v1/", "model": " model "}}}
    ).secondary_stt == SecondarySTT("http://localhost/v1", "model")
    for item in [
        {},
        {"url": "http://localhost", "model": " "},
        {"url": "ftp://localhost", "model": "x"},
        {"url": "http://user@localhost", "model": "x"},
        {"url": "http://localhost?x", "model": "x"},
        {"url": "http://localhost#x", "model": "x"},
        {"url": "http://localhost:not-a-port", "model": "x"},
        {"url": "http://localhost:99999", "model": "x"},
        {"url": "http://localhost", "model": "x", "api_key": 1},
        {"url": "http://localhost", "model": "x", "extra": 1},
    ]:
        with pytest.raises(ValueError):
            load({"stt": {"secondary": item}})
    with pytest.raises(ValueError):
        load({"stt": {"unknown": {}}})


def test_disabled_secondary_does_not_buffer_or_call(config, monkeypatch):
    from thalvi_desktop import client

    async def events(*args):
        assert args[-1] is None
        return Request(Ending.PASTE, "live")

    async def unexpected(*_args):
        pytest.fail("Disabled secondary must not run")

    monkeypatch.setattr(client, "_capture_events", events)
    monkeypatch.setattr(client, "secondary_transcription", unexpected)

    async def behavior():
        async def server(socket):
            await socket.send('{"type":"ready"}')
            await socket.wait_closed()

        async with serve(server, "127.0.0.1", 0) as listener:
            local = replace(config, server=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}")
            assert await capture(local, FakeAudio(), Capture(), lambda _: None) == Request(
                Ending.PASTE, "live"
            )

    asyncio.run(behavior())
