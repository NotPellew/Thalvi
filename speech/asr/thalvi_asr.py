"""Thalvi's self-hosted live recognizer: a streaming model behind the realtime ASR protocol of STT_URL."""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import re
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Protocol

import numpy as np
from websockets.asyncio.server import ServerConnection, serve
from websockets.exceptions import ConnectionClosed

log = logging.getLogger("thalvi-asr")

INPUT_RATE = 24_000
MODEL_RATE = 16_000
# The model takes audio in steps of its chunk size (0.16 s).
STEP_SAMPLES = 2_560
HANDSHAKE_TIMEOUT_S = 10.0
MAX_PHRASES = 100
MAX_PHRASE_CHARS = 40


LANGUAGE_TAG = re.compile(r"\s*<[a-z]{2}(-[A-Z]{2})?>")
# The unknown token reads <unk> in partials and ⁇ in finals; the model can repeat it in bursts on a
# non-speech sound.
UNKNOWN = re.compile(r"\s*(?:\u2047|<unk>)\s*")


def clean(text: str) -> str:
    """The model's text without language tags or unknown tokens, neither of which is something said."""
    return UNKNOWN.sub(" ", LANGUAGE_TAG.sub("", text)).strip()


@dataclass(frozen=True)
class Step:
    """What one step of a stream produced: the text since the last endpoint, and a stretch the endpoint just closed."""

    partial: str
    final: str


class Stream(Protocol):
    def step(self, samples: np.ndarray, last: bool) -> Step:
        """Takes STEP_SAMPLES of 16 kHz audio scaled to -1..1; `last` ends the stream and frees it."""
        ...


class Model(Protocol):
    def open(self, phrases: tuple[str, ...]) -> Stream:
        """Starts a stream that favours `phrases` while decoding."""
        ...


class Resampler:
    """Turns 24 kHz into 16 kHz across chunk boundaries: up by 2, low-pass, down by 3."""

    TAPS = 96

    def __init__(self) -> None:
        n = np.arange(self.TAPS) - (self.TAPS - 1) / 2
        cutoff = 0.9 * MODEL_RATE / 2 / (2 * INPUT_RATE)
        kernel = np.sinc(2 * cutoff * n) * np.hamming(self.TAPS)
        # Gain 2 makes up for the zeros stuffed in when upsampling.
        self.kernel = (2 * kernel / kernel.sum()).astype(np.float32)
        self.history = np.zeros(self.TAPS - 1, np.float32)
        self.phase = 0

    def push(self, samples: np.ndarray) -> np.ndarray:
        up = np.zeros(samples.size * 2, np.float32)
        up[::2] = samples
        padded = np.concatenate([self.history, up])
        filtered = np.convolve(padded, self.kernel, mode="valid")
        self.history = padded[-(self.TAPS - 1) :]
        out = filtered[self.phase :: 3]
        self.phase = (self.phase - filtered.size) % 3
        return out


def spoken(text: str) -> str:
    """`text` trimmed, or nothing when it holds no word: punctuation alone is no speech to start a turn."""
    text = text.strip()
    return text if any(char.isalnum() for char in text) else ""


@dataclass
class Turns:
    """Turns step results into the protocol's speechStart, transcript, speechEnd and speechComplete events."""

    turn: int = 0
    speaking: bool = False
    shown: str = ""
    events: list[dict] = field(default_factory=list)

    def take(self, step: Step, processed_ms: int) -> list[dict]:
        events: list[dict] = []
        partial, final = spoken(step.partial), spoken(step.final)
        if final:
            # A final can arrive in the same step its first words do.
            self._start(events, processed_ms)
            events.append({"type": "speechEnd", "turnId": self._id(), "audioProcessedMs": processed_ms})
            events.append({"type": "speechComplete", "turnId": self._id(), "transcript": final})
            self.turn += 1
            self.speaking = False
            self.shown = ""
        if partial and partial != self.shown:
            self._start(events, processed_ms)
            events.append({"type": "transcript", "turnId": self._id(), "transcript": partial})
            self.shown = partial
        return events

    def _start(self, events: list[dict], processed_ms: int) -> None:
        if not self.speaking:
            self.speaking = True
            events.append({"type": "speechStart", "turnId": self._id(), "audioProcessedMs": processed_ms})

    def _id(self) -> str:
        return f"turn-{self.turn}"


def handshake_error(init: object) -> str | None:
    """Why the server's opening message can't start a session, in plain words; None when it can."""
    if not isinstance(init, dict):
        return "The first message must be a JSON object."
    if init.get("audioEncoding") != "PCM_24KHZ":
        return "Only PCM_24KHZ audio is supported."
    if init.get("mode") != "ENDPOINTING":
        return "Only ENDPOINTING mode is supported."
    return None


def phrases_error(phrases: object) -> str | None:
    """Why a word list can't be used, in plain words; None when it can."""
    if not isinstance(phrases, list) or not all(isinstance(phrase, str) for phrase in phrases):
        return "The word list must be a list of text."
    if len(phrases) > MAX_PHRASES:
        return f"The word list can hold at most {MAX_PHRASES} entries."
    if any(not phrase.strip() or len(phrase.strip()) > MAX_PHRASE_CHARS for phrase in phrases):
        return f"Each word list entry must be 1 to {MAX_PHRASE_CHARS} characters."
    return None


@dataclass
class Session:
    """One connection's audio, from 24 kHz chunks to steps of the model's stream."""

    stream: Stream
    resampler: Resampler = field(default_factory=Resampler)
    pending: np.ndarray = field(default_factory=lambda: np.zeros(0, np.float32))
    turns: Turns = field(default_factory=Turns)
    received: int = 0
    step_ms: list[float] = field(default_factory=list)

    def chunks(self, pcm: bytes) -> list[np.ndarray]:
        samples = np.frombuffer(pcm, dtype="<i2").astype(np.float32) / 32768
        self.received += samples.size
        self.pending = np.concatenate([self.pending, self.resampler.push(samples)])
        whole = self.pending.size // STEP_SAMPLES * STEP_SAMPLES
        steps = [self.pending[i : i + STEP_SAMPLES] for i in range(0, whole, STEP_SAMPLES)]
        self.pending = self.pending[whole:]
        return steps

    def tail(self) -> np.ndarray:
        last = np.zeros(STEP_SAMPLES, np.float32)
        last[: self.pending.size] = self.pending
        self.pending = np.zeros(0, np.float32)
        return last

    def run(self, samples: np.ndarray, last: bool) -> list[dict]:
        started = time.perf_counter()
        step = self.stream.step(samples, last)
        self.step_ms.append((time.perf_counter() - started) * 1000)
        return self.turns.take(step, self.received * 1000 // INPUT_RATE)


class Server:
    """Serves the model; one worker thread runs every stream's steps so the GPU sees one call at a time."""

    def __init__(self, model: Model) -> None:
        self.model = model
        self.worker = ThreadPoolExecutor(max_workers=1, thread_name_prefix="asr-model")

    async def _open(self, phrases: tuple[str, ...]) -> Stream:
        return await asyncio.get_running_loop().run_in_executor(self.worker, self.model.open, phrases)

    async def _run(self, session: Session, samples: np.ndarray, last: bool) -> list[dict]:
        return await asyncio.get_running_loop().run_in_executor(self.worker, session.run, samples, last)

    async def handle(self, socket: ServerConnection) -> None:
        connected = time.perf_counter()
        try:
            init = json.loads(await asyncio.wait_for(socket.recv(), HANDSHAKE_TIMEOUT_S))
        except (TimeoutError, json.JSONDecodeError, ConnectionClosed):
            await self._fail(socket, "The session did not start with a JSON handshake.")
            return
        if error := handshake_error(init):
            await self._fail(socket, error)
            return
        await socket.send(json.dumps({"sessionId": str(uuid.uuid4()), "phraseBoosting": True}))
        phrases: tuple[str, ...] = ()
        session: Session | None = None
        ended, outcome = False, "closed"
        try:
            async for message in socket:
                if isinstance(message, bytes):
                    if len(message) % 2:
                        await self._fail(socket, "Audio must be whole 16-bit samples.")
                        outcome = "bad_audio"
                        break
                    # The stream starts with the first audio so a word list sent before it applies.
                    if session is None:
                        session = Session(await self._open(phrases))
                    for samples in session.chunks(message):
                        for event in await self._run(session, samples, last=False):
                            await socket.send(json.dumps(event))
                    continue
                command = json.loads(message)
                if command.get("type") == "phrases":
                    error = "Send the word list before any audio." if session else phrases_error(command.get("phrases"))
                    if error:
                        await self._fail(socket, error)
                        outcome = "bad_phrases"
                        break
                    phrases = tuple(phrase.strip() for phrase in command["phrases"])
                elif command.get("type") == "endStream":
                    if session:
                        for event in await self._run(session, session.tail(), last=True):
                            await socket.send(json.dumps(event))
                    ended, outcome = True, "ended"
                    break
        except ConnectionClosed:
            pass
        except Exception:
            log.exception("recognition failed")
            outcome = "error"
            await self._fail(socket, "Speech recognition failed on the recognizer.")
        finally:
            if session and not ended:
                await self._run(session, session.tail(), last=True)
            self._log(session, outcome, connected, len(phrases))
        await socket.close()

    async def _fail(self, socket: ServerConnection, message: str) -> None:
        try:
            await socket.send(json.dumps({"type": "error", "message": message}))
            await socket.close()
        except ConnectionClosed:
            pass

    @staticmethod
    def _log(session: Session | None, outcome: str, connected: float, phrases: int) -> None:
        step_ms = session.step_ms if session else []
        steps = np.array(step_ms or [0.0])
        log.info(
            "session outcome=%s phrases=%d audio_ms=%d steps=%d step_p50_ms=%.1f step_p95_ms=%.1f total_ms=%d",
            outcome,
            phrases,
            session.received * 1000 // INPUT_RATE if session else 0,
            len(step_ms),
            float(np.percentile(steps, 50)),
            float(np.percentile(steps, 95)),
            (time.perf_counter() - connected) * 1000,
        )


async def run(model: Model, host: str, port: int) -> None:
    server = Server(model)
    async with serve(server.handle, host, port, max_size=1 << 20) as running:
        log.info("listening on ws://%s:%d", host, port)
        await running.serve_forever()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default=os.environ.get("HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int, default=int(os.environ.get("PORT", "8012")))
    parser.add_argument("--model", default=os.environ.get("ASR_MODEL"), required="ASR_MODEL" not in os.environ)
    parser.add_argument("--endpoint-ms", type=int, default=int(os.environ.get("ENDPOINT_MS", "600")))
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    from nemo_stream import NemoModel

    started = time.perf_counter()
    model = NemoModel(args.model, args.endpoint_ms)
    log.info("model loaded in %.1f s", time.perf_counter() - started)
    asyncio.run(run(model, args.host, args.port))


if __name__ == "__main__":
    main()
