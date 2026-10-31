"""Owns the native capture and conversation sockets to the local Rust server."""

import asyncio
import json
import time
from collections.abc import Callable
from contextlib import suppress

from websockets.asyncio.client import connect

from .audio import Audio, transcription_pcm
from .capture import Capture, Request
from .config import Config, record
from .metrics import Metrics


async def capture(
    config: Config,
    audio: Audio,
    current: Capture,
    notice: Callable[[str], None],
    ready_message: str = "Recording. Speak, then finish with an ending command.",
    metrics: Metrics | None = None,
    confirmation: bool = False,
) -> Request:
    audio.set_input(False)
    socket = None
    connection = None
    activation = None
    started = time.monotonic()
    connection_outcome = "error"

    async def activate() -> None:
        cue_started = time.monotonic()
        outcome = "error"
        try:
            await audio.cue(880 if confirmation else 440)
            audio.set_input(True)
            notice(ready_message)
            outcome = "ok"
        except asyncio.CancelledError:
            outcome = "cancelled"
            raise
        finally:
            if metrics:
                with suppress(OSError):
                    await metrics.record(
                        "desktop_ready_cue" if confirmation else "desktop_start_cue",
                        cue_started,
                        outcome,
                        buffered=True,
                    )

    async def open_ready():
        nonlocal socket, connection_outcome
        try:
            socket = await connect(
                config.server + "/transcription", proxy=None, close_timeout=2, max_size=100_000
            )
            raw = await socket.recv()
            ready = record(json.loads(raw), "transcription response")
            if ready.get("type") != "ready":
                raise RuntimeError(str(ready.get("message", "Speech recognition could not start.")))
            connection_outcome = "ok"
        except asyncio.CancelledError:
            connection_outcome = "cancelled"
            raise
        finally:
            if metrics:
                with suppress(OSError):
                    await metrics.record(
                        "desktop_transcription_ready", started, connection_outcome, buffered=True
                    )

    try:
        async with asyncio.timeout(8):
            connection = asyncio.create_task(open_ready())
            if confirmation:
                await connection
                activation = asyncio.create_task(activate())
                await activation
            else:
                activation = asyncio.create_task(activate())
                await asyncio.gather(connection, activation)
        return await _capture_events(socket, audio, current, notice)
    finally:
        tasks = [task for task in (connection, activation) if task is not None]
        for task in tasks:
            if not task.done():
                task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if socket is not None:
            await socket.close()
        audio.set_input(True)


async def _capture_events(socket, audio, current, notice):
    async def send_audio() -> None:
        while True:
            await socket.send(transcription_pcm(await audio.next()))

    async def receive() -> Request:
        async for raw in socket:
            if not isinstance(raw, str):
                raise RuntimeError("Transcription returned audio instead of text.")
            message = record(json.loads(raw), "transcription event")
            match message.get("type"):
                case "final":
                    text = message.get("text")
                    if not isinstance(text, str):
                        raise RuntimeError("Transcription returned an invalid final turn.")
                    if request := current.final(text):
                        return request
                    notice("Capturing · " + current.text)
                case "error":
                    raise RuntimeError(str(message.get("message", "Speech recognition failed.")))
                case "partial":
                    if not isinstance(message.get("text"), str):
                        raise RuntimeError("Transcription returned an invalid partial turn.")
                    notice("Hearing (unconfirmed) · " + message["text"])
                case _:
                    raise RuntimeError("Transcription returned an unknown event.")
        raise RuntimeError("Speech recognition disconnected. Nothing was delivered.")

    sender, receiver = asyncio.create_task(send_audio()), asyncio.create_task(receive())
    try:
        done, _ = await asyncio.wait({sender, receiver}, return_when=asyncio.FIRST_COMPLETED)
        if sender in done:
            await sender
            raise RuntimeError("Microphone audio ended. Nothing was delivered.")
        return await receiver
    finally:
        sender.cancel()
        receiver.cancel()
        await asyncio.gather(sender, receiver, return_exceptions=True)


async def conversation(config: Config, audio: Audio, notice: Callable[[str], None]) -> None:
    audio.new_conversation()
    async with connect(
        config.server + "/session", proxy=None, close_timeout=2, max_size=1_000_000
    ) as socket:
        await socket.send(json.dumps({"type": "voice", "on": True}))
        last_reported = -1
        ready = asyncio.Event()

        async def send_audio() -> None:
            nonlocal last_reported
            await ready.wait()
            while True:
                await socket.send(transcription_pcm(await audio.next()))
                drained = audio.drained_bytes()
                if drained is not None and last_reported != drained:
                    last_reported = drained
                    await socket.send(json.dumps({"type": "played", "bytes": drained}))

        async def receive() -> None:
            reply = ""
            async for raw in socket:
                if isinstance(raw, bytes):
                    audio.play(raw)
                    continue
                message = record(json.loads(raw), "conversation event")
                kind = message.get("type")
                if kind == "state":
                    notice("Conversation · " + str(message.get("state", "working")))
                    if message.get("state") == "listening":
                        ready.set()
                elif kind == "interrupted":
                    audio.stop()
                    reply = ""
                    notice("Listening after your interruption.")
                elif kind == "user":
                    text = message.get("text")
                    if not isinstance(text, str):
                        raise RuntimeError("Conversation returned an invalid transcript.")
                    if text.strip(" .!?").casefold() in {
                        "stop conversation",
                        "end conversation",
                        "never mind",
                    }:
                        await socket.send(json.dumps({"type": "cancel"}))
                        return
                    notice("You · " + text)
                elif kind == "error":
                    notice(str(message.get("message", "Conversation failed.")))
                elif kind == "reply":
                    text = message.get("text")
                    if isinstance(text, str):
                        reply += text
                elif kind == "reply_end":
                    if reply:
                        notice("Thalvi · " + reply)
                    reply = ""
                elif kind == "voice" and message.get("on") is False:
                    notice("Conversation voice stopped. Returning to wake-word listening.")
                    return
                elif kind == "ended":
                    return
            raise RuntimeError("The conversation connection closed.")

        sender, receiver = asyncio.create_task(send_audio()), asyncio.create_task(receive())
        try:
            done, _ = await asyncio.wait({sender, receiver}, return_when=asyncio.FIRST_COMPLETED)
            for task in done:
                await task
        finally:
            sender.cancel()
            receiver.cancel()
            await asyncio.gather(sender, receiver, return_exceptions=True)
            audio.stop()
            with suppress(Exception):
                await socket.send(json.dumps({"type": "voice", "on": False}))
