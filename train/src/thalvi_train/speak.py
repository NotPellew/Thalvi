"""Turns a questions JSONL into Kokoro speech and a manifest; run as `python -m thalvi_train.speak`.

Voice and speed come from a hash of the text, so reruns pick the same ones and
skip audio that already exists. Every fifth English voice, in sorted order, is
held out: its samples form the `eval` split. Audio keeps Kokoro's own sample rate.
"""

import argparse
import asyncio
import hashlib
import io
import sys
from dataclasses import dataclass
from pathlib import Path

import httpx
import soundfile

from thalvi_train.http_retry import Sleep, post_with_retry
from thalvi_train.manifest import Sample, Split, write_manifest
from thalvi_train.questions import Question, dedupe_key, read_questions

DEFAULT_URL = "http://127.0.0.1:8881"
ENGLISH_PREFIXES = ("a", "b")
EVAL_VOICE_STRIDE = 5
SPEED_RANGE = (0.9, 1.15)
MANIFEST_NAME = "manifest.jsonl"
AUDIO_ATTEMPTS = 3


class SpeakError(RuntimeError):
    """The speech server is unreachable or returned something unusable."""


@dataclass(frozen=True)
class Plan:
    """What to synthesize for one request."""

    id: str
    text: str
    voice: str
    speed: float
    split: Split


def english_voices(ids: list[str]) -> list[str]:
    """The sorted English voice ids among `ids`."""
    return sorted(v for v in ids if v.startswith(ENGLISH_PREFIXES))


def held_out_voices(voices: list[str]) -> set[str]:
    """Every fifth voice of the sorted list, kept for evaluation."""
    return set(voices[::EVAL_VOICE_STRIDE])


def plan_for(text: str, voices: list[str]) -> Plan:
    """Picks voice and speed for `text` from its hash, and the split from the voice."""
    digest = hashlib.sha256(text.encode()).digest()
    voice = voices[int.from_bytes(digest[:4]) % len(voices)]
    low, high = SPEED_RANGE
    speed = round(low + (high - low) * int.from_bytes(digest[4:8]) / 0xFFFFFFFF, 2)
    split = "eval" if voice in held_out_voices(voices) else "train"
    return Plan(id=f"q-{digest.hex()[:12]}", text=text, voice=voice, speed=speed, split=split)


def check_wav(data: bytes) -> float:
    """Duration in seconds of `data`, which must be non-empty WAV audio."""
    try:
        info = soundfile.info(io.BytesIO(data))
    except soundfile.LibsndfileError as error:
        raise SpeakError(f"response is not audio: {error}") from error
    if info.format != "WAV" or info.frames == 0:
        raise SpeakError(f"response is {info.format} with {info.frames} frames, not WAV audio")
    return info.duration


async def fetch_voices(client: httpx.AsyncClient, base_url: str) -> list[str]:
    """English voice ids offered by the server."""
    response = await client.get(f"{base_url}/v1/audio/voices")
    response.raise_for_status()
    try:
        entries = response.json()["voices"]
        ids = [entry["id"] if isinstance(entry, dict) else entry for entry in entries]
    except (ValueError, KeyError, TypeError) as error:
        raise SpeakError(f"unexpected voices response: {error!r}") from error
    voices = english_voices(ids)
    if not voices:
        raise SpeakError("the server offers no English voices")
    return voices


async def synthesize(
    client: httpx.AsyncClient,
    base_url: str,
    plan: Plan,
    dest: Path,
    sleep: Sleep,
    base_delay: float,
) -> float:
    """Writes the speech for `plan` to `dest` atomically and returns its duration.

    Kokoro-FastAPI sometimes answers 200 with an empty body when a voice is first
    loaded under load, so a reply that is not WAV audio is requested again.
    """
    for attempt in range(AUDIO_ATTEMPTS):
        response = await post_with_retry(
            client,
            f"{base_url}/v1/audio/speech",
            json={
                "model": "kokoro",
                "input": plan.text,
                "voice": plan.voice,
                "response_format": "wav",
                "speed": plan.speed,
            },
            sleep=sleep,
            base_delay=base_delay,
        )
        response.raise_for_status()
        try:
            duration = check_wav(response.content)
        except SpeakError:
            if attempt == AUDIO_ATTEMPTS - 1:
                raise
            continue
        partial = dest.with_suffix(".part")
        partial.write_bytes(response.content)
        partial.replace(dest)
        return duration
    raise AssertionError("unreachable")


async def speak(
    questions: list[Question],
    out_dir: Path,
    *,
    base_url: str = DEFAULT_URL,
    concurrency: int = 4,
    transport: httpx.AsyncBaseTransport | None = None,
    sleep: Sleep = asyncio.sleep,
    base_delay: float = 1.0,
) -> tuple[int, list[str]]:
    """Synthesizes missing audio under `out_dir/audio` and writes `out_dir/manifest.jsonl`.

    Returns the number of samples in the manifest and the ids that failed; failed
    requests are left out of the manifest and retried by the next run.
    """
    audio_dir = out_dir / "audio"
    audio_dir.mkdir(parents=True, exist_ok=True)
    unique = {dedupe_key(q.text): q for q in questions}.values()
    gate = asyncio.Semaphore(concurrency)
    failed: list[str] = []
    async with httpx.AsyncClient(transport=transport, timeout=120.0) as client:
        voices = await fetch_voices(client, base_url)
        plans = [plan_for(q.text, voices) for q in unique]

        async def one(plan: Plan) -> None:
            dest = audio_dir / f"{plan.id}.wav"
            if dest.exists():
                return
            async with gate:
                try:
                    await synthesize(client, base_url, plan, dest, sleep, base_delay)
                except (httpx.HTTPError, SpeakError) as error:
                    print(f"{plan.id} failed: {error!r}", file=sys.stderr)
                    failed.append(plan.id)

        await asyncio.gather(*(one(plan) for plan in plans))

    samples = [
        Sample(
            id=plan.id,
            audio=f"audio/{plan.id}.wav",
            text=plan.text,
            task="answer",
            split=plan.split,
            voice=plan.voice,
            source="kokoro",
        )
        for plan in plans
        if (audio_dir / f"{plan.id}.wav").exists()
    ]
    write_manifest(out_dir / MANIFEST_NAME, samples)
    return len(samples), failed


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("questions", type=Path, help="questions JSONL from thalvi_train.questions")
    parser.add_argument("out_dir", type=Path, help="receives audio/ and manifest.jsonl")
    parser.add_argument("--base-url", default=DEFAULT_URL, help="Kokoro-FastAPI address")
    parser.add_argument("--concurrency", type=int, default=4)
    args = parser.parse_args(argv)
    try:
        count, failed = asyncio.run(
            speak(
                read_questions(args.questions),
                args.out_dir,
                base_url=args.base_url,
                concurrency=args.concurrency,
            )
        )
    except (httpx.HTTPError, SpeakError) as error:
        print(f"error: {error!r}", file=sys.stderr)
        return 1
    print(f"{count} samples in {args.out_dir / MANIFEST_NAME}; {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
