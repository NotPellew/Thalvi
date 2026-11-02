"""Owns the native capture and conversation sockets to the local Rust server."""

import asyncio
import http.client
import io
import json
import time
import urllib.error
import urllib.request
import uuid
import wave
from collections import deque
from collections.abc import Callable
from contextlib import suppress

from websockets.asyncio.client import connect

from .audio import Audio, transcription_pcm
from .capture import Capture, Ending, Request, trailing_artifacts
from .config import Config, SecondaryAPI, SecondarySTT, record
from .cues import Cue, Cues
from .metrics import Metrics
from .pending import Pending
from .vad import VoiceActivity


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
            if config.cues.enabled(Cue.WAKE):
                await audio.play_cue(Cue.WAKE)
            audio.set_input(True)
            notice(ready_message)
            outcome = "ok"
        except asyncio.CancelledError:
            outcome = "cancelled"
            raise
        finally:
            if metrics:
                with suppress(OSError):
                    await metrics.record("desktop_ready_cue", cue_started, outcome, buffered=True)

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
            await connection
        pcm = bytearray() if config.secondary_stt and not confirmation else None
        request = await _capture_events(
            socket,
            audio,
            current,
            notice,
            metrics,
            confirmation,
            config.ending_pause,
            pending_notice,
            config.cues,
            pcm,
        )
        if (
            pcm is not None
            and not current.scratched
            and request.ending in {Ending.PASTE, Ending.SUBMIT, Ending.TASK}
        ):
            request = await secondary_transcription(
                config.secondary_stt, bytes(pcm), request, current.endings, notice, metrics
            )
        return request
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
    cues: Cues | None = None,
    pcm: bytearray | None = None,
):
    cues = cues or Cues()
    gate = None if confirmation else Pending(current, pause)
    vad = None if confirmation else VoiceActivity()
    changed = asyncio.Event()
    active_cues = 0
    cues_idle = asyncio.Event()
    cues_idle.set()
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

    async def optional_cue(cue: Cue) -> None:
        nonlocal active_cues
        if not cues.enabled(cue):
            return
        active_cues += 1
        cues_idle.clear()
        frame_changed.set()
        started = time.monotonic()
        try:
            await audio.play_cue(cue)
        except RuntimeError as error:
            notice(f"{error} Capture continues.")
            return
        finally:
            active_cues -= 1
            if not active_cues:
                cues_idle.set()
            frame_changed.set()
        if metrics:
            with suppress(OSError):
                await metrics.record("desktop_cue", started, cue.value, buffered=True)

    async def send_audio() -> None:
        nonlocal frame_seq, local_speech
        continuation_frames = 0
        held_frames = deque(maxlen=3)
        relay_request = None
        while True:
            frame = await audio.next()
            if gate is not None and gate.capture.finished:
                continue
            outgoing = [frame]
            if gate is not None:
                speech = vad.speech(frame, pending=gate.request is not None)
                local_speech = speech
                if gate.request is not None:
                    if relay_request is not None and relay_request is not gate.request:
                        relay_request = None
                        continuation_frames = 0
                        held_frames.clear()
                    held_frames.append(frame)
                    continuation_frames = continuation_frames + 1 if speech else 0
                    if gate.waiting_for_quiet and not speech:
                        gate.waiting_for_quiet = False
                        gate.since = asyncio.get_running_loop().time()
                        changed.set()
                        if pending_notice:
                            pending_notice(gate.request, pause)
                        await diagnostic("quiet_started")
                    if continuation_frames == 3:
                        relay_request = gate.request
                        await diagnostic("local_speech_relayed")
                    if relay_request is None:
                        frame_seq += 1
                        frame_changed.set()
                        continue
                    if continuation_frames == 3:
                        outgoing = list(held_frames)
                else:
                    continuation_frames = 0
                    if held_frames and relay_request is None:
                        held_frames.append(frame)
                        outgoing = list(held_frames)
                    relay_request = None
                    held_frames.clear()
            frame_seq += 1
            frame_changed.set()
            for buffered in outgoing:
                frame_pcm = transcription_pcm(buffered)
                await socket.send(frame_pcm)
                if pcm is not None:
                    pcm.extend(frame_pcm)

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
                            if gate.request is not previous:
                                await optional_cue(Cue.PROVISIONAL)
                    if request:
                        return request
                    notice("Capturing · " + current.text)
                    if gate is not None:
                        if previous is not None and text.strip():
                            await diagnostic("final_cancelled")
                        if previous is not None and gate.request is None:
                            await optional_cue(Cue.CANCELLED)
                        if gate.request is not None:
                            await diagnostic(
                                "waiting_for_quiet" if gate.waiting_for_quiet else "pending"
                            )
                case "error":
                    raise RuntimeError(str(message.get("message", "Speech recognition failed.")))
                case "speech_start" | "speech_end":
                    pass
                case "partial":
                    if not isinstance(message.get("text"), str):
                        raise RuntimeError("Transcription returned an invalid partial turn.")
                    if (
                        message["text"].strip()
                        and not trailing_artifacts(message["text"])
                        and gate is not None
                        and gate.request is not None
                    ):
                        gate.continuation()
                        changed.set()
                        frame_changed.set()
                        if pending_notice:
                            pending_notice(None, pause)
                        await diagnostic("partial_cancelled")
                        await optional_cue(Cue.CANCELLED)
                    notice("Hearing (unconfirmed) · " + message["text"])
                case _:
                    raise RuntimeError("Transcription returned an unknown event.")
        raise RuntimeError("Speech recognition disconnected. Nothing was delivered.")

    async def wait_for_frame(candidate):
        seen = frame_seq
        while frame_seq == seen and gate.request is candidate:
            frame_changed.clear()
            if active_cues:
                await cues_idle.wait()
                continue
            try:
                async with asyncio.timeout(0.25):
                    await frame_changed.wait()
            except TimeoutError as error:
                raise RuntimeError("Microphone audio stopped. Nothing was delivered.") from error

    async def wait_for_pause():
        while True:
            changed.clear()
            candidate = gate.request
            if candidate is None:
                await changed.wait()
                continue
            if gate.waiting_for_quiet:
                await wait_for_frame(candidate)
                continue
            remaining = gate.since + gate.seconds - asyncio.get_running_loop().time()
            if remaining > 0:
                with suppress(TimeoutError):
                    async with asyncio.timeout(remaining):
                        await changed.wait()
                continue
            await wait_for_frame(candidate)
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
        end_cue = Cue.CANCELLED if request.ending == Ending.CANCEL else Cue.ACCEPTED
        if not confirmation and cues.enabled(end_cue):
            started = time.monotonic()
            outcome = "ok"
            try:
                await audio.play_cue(end_cue)
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


def _secondary_request(settings: SecondarySTT, pcm: bytes) -> str:
    output = io.BytesIO()
    with wave.open(output, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(24000)
        wav.writeframes(pcm)
    boundary = uuid.uuid4().hex
    if settings.api == SecondaryAPI.META:
        endpoint, audio_field, text_field = "/asr/transcribe", "audio", "transcript"
        parameters = (
            'Content-Disposition: form-data; name="request"\r\n'
            "Content-Type: application/json\r\n\r\n"
            + json.dumps({"mode": "PUSH_TO_TALK", "model": settings.model, "audioEncoding": "WAV"})
        )
    else:
        endpoint, audio_field, text_field = "/audio/transcriptions", "file", "text"
        parameters = 'Content-Disposition: form-data; name="model"\r\n\r\n' + settings.model
    body = (
        (
            f"--{boundary}\r\n{parameters}\r\n--{boundary}\r\n"
            f'Content-Disposition: form-data; name="{audio_field}"; filename="capture.wav"\r\n'
            "Content-Type: audio/wav\r\n\r\n"
        ).encode()
        + output.getvalue()
        + f"\r\n--{boundary}--\r\n".encode()
    )
    headers = {
        "Content-Type": f"multipart/form-data; boundary={boundary}",
        "Accept": "application/json",
    }
    if settings.api_key:
        headers["Authorization"] = "Bearer " + settings.api_key
    request = urllib.request.Request(settings.url + endpoint, body, headers)
    with urllib.request.urlopen(request, timeout=10) as response:
        result = json.load(response)
    if not isinstance(result, dict) or not isinstance(result.get(text_field), str):
        raise ValueError("Invalid transcription response")
    return result[text_field]


async def secondary_transcription(settings, pcm, request, endings, notice, metrics):
    started = time.monotonic()
    outcome = "error"
    try:
        async with asyncio.timeout(10):
            transcript = await asyncio.to_thread(_secondary_request, settings, pcm)
        parser = Capture(endings)
        ending, _ = parser.ending(transcript.strip())
        if ending is not None and ending != request.ending:
            raise ValueError("Secondary ending differs")
        parsed = parser.final(transcript)
        if parsed is None and not transcript.strip():
            raise ValueError("Empty transcription")
        outcome = "ok"
        return Request(
            request.ending, parsed.text if parsed else transcript.strip(), request.pause_confirmed
        )
    except asyncio.CancelledError:
        outcome = "cancelled"
        raise
    except (TimeoutError, urllib.error.URLError) as error:
        if isinstance(error, TimeoutError) or isinstance(error.reason, TimeoutError):
            outcome = "timeout"
    except (OSError, ValueError, UnicodeError, json.JSONDecodeError, http.client.HTTPException):
        pass
    finally:
        if metrics:
            with suppress(Exception):
                await metrics.record("desktop_secondary_stt", started, outcome, buffered=True)
    notice("The second transcription failed. Using the live transcript.")
    return request


async def conversation(config: Config, audio: Audio, notice: Callable[[str], None]) -> None:
    audio.new_conversation()
    if config.cues.enabled(Cue.WAKE):
        await audio.play_cue(Cue.WAKE)
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
