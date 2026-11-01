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
from .pending import Pending
from .vad import VoiceActivity

PENDING_CUE_HZ = 520
CANCELLED_CUE_HZ = 330


async def capture(
    config: Config,
    audio: Audio,
    current: Capture,
    notice: Callable[[str], None],
    ready_message: str = "Recording. Speak, then finish with an ending command.",
    metrics: Metrics | None = None,
    confirmation: bool = False,
    pending_notice: Callable[[Request, float], None] | None = None,
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
            if confirmation:
                await audio.cue(880)
            else:
                await audio.cue(440, duration=0.04)
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
        return await _capture_events(
            socket,
            audio,
            current,
            notice,
            metrics,
            confirmation,
            config.ending_pause,
            pending_notice,
        )
    finally:
        tasks = [task for task in (connection, activation) if task is not None]
        for task in tasks:
            if not task.done():
                task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if socket is not None:
            await socket.close()
        audio.set_input(True)


async def _capture_events(
    socket,
    audio,
    current,
    notice,
    metrics=None,
    confirmation=False,
    pause=2.5,
    pending_notice=None,
):
    gate = None if confirmation else Pending(current, pause)
    vad = None if confirmation else VoiceActivity()
    changed = asyncio.Event()
    frame_seq = 0
    frame_changed = asyncio.Event()
    pause_outcome = "cancelled"
    local_speech = False

    async def diagnostic(outcome):
        if metrics is None:
            return
        try:
            await metrics.record("desktop_ending_state", time.monotonic(), outcome, buffered=True)
        except OSError:
            notice("The optional ending diagnostics could not be saved.")

    cued: Request | None = None
    cue_active = False
    feedback_lock = asyncio.Lock()

    async def feedback(frequency: int, event: str) -> None:
        nonlocal cue_active
        async with feedback_lock:
            started = time.monotonic()
            outcome = "ok"
            cue_active = True
            try:
                await audio.cue(frequency, duration=0.08)
            except asyncio.CancelledError:
                outcome = "cancelled"
                raise
            except (RuntimeError, OSError) as error:
                outcome = "error"
                notice(f"{error} Capture continues.")
            finally:
                cue_active = False
                if metrics:
                    with suppress(OSError):
                        await metrics.record(event, started, outcome, buffered=True)

    async def note_gate_change(before: Request | None) -> None:
        nonlocal cued
        if gate is None:
            return
        if gate.request is not None and gate.request is not cued:
            cued = gate.request
            await feedback(PENDING_CUE_HZ, "desktop_pending_cue")
        elif gate.request is None and before is not None and cued is before:
            cued = None
            await feedback(CANCELLED_CUE_HZ, "desktop_cancel_cue")

    async def send_audio() -> None:
        nonlocal frame_seq, local_speech
        while True:
            frame = await audio.next()
            if cue_active:
                frame_seq += 1
                frame_changed.set()
                continue
            if gate is not None:
                speech = vad.speech(frame)
                previous_speech, local_speech = local_speech, speech
                if gate.request is not None:
                    if gate.waiting_for_quiet and not speech:
                        gate.waiting_for_quiet = False
                        gate.since = asyncio.get_running_loop().time()
                        changed.set()
                        if pending_notice:
                            pending_notice(gate.request, pause)
                        await diagnostic("quiet_started")
                    elif speech and not previous_speech and not gate.waiting_for_quiet:
                        before = gate.request
                        gate.continuation()
                        changed.set()
                        if pending_notice:
                            pending_notice(None, pause)
                        notice("Speech continued. Keep speaking or say an ending again.")
                        await diagnostic("local_speech_cancelled")
                        await note_gate_change(before)
            frame_seq += 1
            frame_changed.set()
            await socket.send(transcription_pcm(frame))

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
                    if gate is None:
                        request = current.final(text)
                    else:
                        previous = gate.request
                        request = gate.final(
                            text, asyncio.get_running_loop().time(), speech=local_speech
                        )
                        changed.set()
                        frame_changed.set()
                        if previous is not None and gate.request is not previous and pending_notice:
                            pending_notice(None, pause)
                        if gate.request is not None:
                            if pending_notice:
                                pending_notice(gate.request, pause)
                            else:
                                notice(
                                    f"Pause for {pause:g} seconds to confirm {gate.request.ending}."
                                )
                        await note_gate_change(previous)
                    if request:
                        return request
                    notice("Capturing · " + current.text)
                    if gate is not None:
                        if previous is not None and text.strip():
                            await diagnostic("final_cancelled")
                        if gate.request is not None:
                            await diagnostic(
                                "waiting_for_quiet" if gate.waiting_for_quiet else "pending"
                            )
                case "error":
                    raise RuntimeError(str(message.get("message", "Speech recognition failed.")))
                case "speech_start":
                    if gate is not None and gate.request is not None:
                        before = gate.request
                        gate.continuation()
                        changed.set()
                        frame_changed.set()
                        if pending_notice:
                            pending_notice(None, pause)
                        await diagnostic("speech_start_cancelled")
                        await note_gate_change(before)
                case "speech_end":
                    pass
                case "partial":
                    if not isinstance(message.get("text"), str):
                        raise RuntimeError("Transcription returned an invalid partial turn.")
                    if message["text"].strip() and gate is not None and gate.request is not None:
                        before = gate.request
                        gate.continuation()
                        changed.set()
                        frame_changed.set()
                        if pending_notice:
                            pending_notice(None, pause)
                        await diagnostic("partial_cancelled")
                        await note_gate_change(before)
                    notice("Hearing (unconfirmed) · " + message["text"])
                case _:
                    raise RuntimeError("Transcription returned an unknown event.")
        raise RuntimeError("Speech recognition disconnected. Nothing was delivered.")

    async def wait_for_pause():
        while True:
            changed.clear()
            candidate = gate.request
            if candidate is None:
                await changed.wait()
                continue
            if gate.waiting_for_quiet:
                frame_changed.clear()
                try:
                    async with asyncio.timeout(0.25):
                        await frame_changed.wait()
                except TimeoutError as error:
                    raise RuntimeError(
                        "Microphone audio stopped. Nothing was delivered."
                    ) from error
                continue
            remaining = gate.since + gate.seconds - asyncio.get_running_loop().time()
            if remaining > 0:
                with suppress(TimeoutError):
                    async with asyncio.timeout(remaining):
                        await changed.wait()
                continue
            seen = frame_seq
            frame_changed.clear()
            try:
                async with asyncio.timeout(0.25):
                    while frame_seq == seen and gate.request is candidate:
                        await frame_changed.wait()
                        frame_changed.clear()
            except TimeoutError as error:
                raise RuntimeError("Microphone audio stopped. Nothing was delivered.") from error
            await asyncio.sleep(0)
            if gate.request is candidate:
                if hasattr(audio, "frames") and not audio.frames.empty():
                    continue
                return candidate

    sender, receiver = asyncio.create_task(send_audio()), asyncio.create_task(receive())
    timer = asyncio.create_task(wait_for_pause()) if gate else None
    try:
        while True:
            done, _ = await asyncio.wait(
                {sender, receiver} | ({timer} if timer else set()),
                return_when=asyncio.FIRST_COMPLETED,
            )
            if sender in done:
                await sender
                raise RuntimeError("Microphone audio ended. Nothing was delivered.")
            if receiver in done:
                request = await receiver
                break
            candidate = await timer
            await asyncio.sleep(0)
            if sender.done():
                await sender
                raise RuntimeError("Microphone audio ended. Nothing was delivered.")
            if receiver.done():
                request = await receiver
                break
            if gate.request is candidate:
                request = gate.ready(asyncio.get_running_loop().time())
                if request is not None:
                    pause_outcome = "ok"
                    break
            timer = asyncio.create_task(wait_for_pause())
        audio.set_input(False)
        sender.cancel()
        await asyncio.gather(sender, return_exceptions=True)
        if pause_outcome == "ok":
            await diagnostic("accepted")
        if not confirmation:
            started = time.monotonic()
            outcome = "ok"
            try:
                await audio.cue(660)
            except asyncio.CancelledError:
                outcome = "cancelled"
                raise
            except (RuntimeError, OSError) as error:
                outcome = "error"
                notice(f"{error} The accepted action will continue.")
            finally:
                if metrics:
                    with suppress(OSError):
                        await metrics.record("desktop_end_cue", started, outcome, buffered=True)
        return request
    finally:
        sender.cancel()
        receiver.cancel()
        if timer:
            timer.cancel()
        await asyncio.gather(sender, receiver, *([timer] if timer else []), return_exceptions=True)
        if metrics and gate and gate.since:
            with suppress(OSError):
                await metrics.record(
                    "desktop_ending_pause", gate.since, pause_outcome, buffered=True
                )


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
