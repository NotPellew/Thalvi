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

    async def next(self):
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

        def reset_input():
            while not audio.queue.empty():
                audio.queue.get_nowait()

        audio.reset_input = reset_input

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
                    fresh_audio=True,
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
