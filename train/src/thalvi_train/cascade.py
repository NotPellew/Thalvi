"""Cascaded baseline CLI: Muse Voice Transcribe's word error rate on the evaluation's clips.

Run as `python -m thalvi_train.cascade` with the Meta API key in `STT_API_KEY`.

The cascade is the pipeline the adapter replaces: Meta's realtime ASR writes the
transcript, and Glimmer answers the text. Glimmer's answer to a transcript is the
teacher's, so the cascade differs from the text ceiling only where the transcript
is wrong, and its word error rate is the comparison for the adapter's echo. The
clips are the ones `evaluate` scores for the same manifest, `--limit` and
`--augment-prob`, augmented under the same seed, so the two rows of a table are
measured on identical audio.

Each clip goes over its own connection in `ENDPOINTING` mode, as 24 kHz PCM in
100 ms chunks at `--speed` times real time, followed by `TRAILING_SILENCE_S` of
silence so the last turn ends. The transcript is every completed turn, joined;
the stream is done when the service sends its final empty transcript after
`endStream`. `--speed 1` paces the audio like a live speaker, and then
`end_to_transcript_s`, from the last speech sample sent to the last completed turn,
is the latency a speaker sees.
"""

import argparse
import asyncio
import json
import os
import statistics
import time
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import numpy as np
import soxr

from thalvi_train.augment import add_arguments, build_augmenter
from thalvi_train.evaluate import AUGMENT_SEED, SUMMARY_SUFFIX, edit_distance, normalize
from thalvi_train.features import SAMPLE_RATE, read_audio
from thalvi_train.records import read_records, resolve_audio

ASR_URL = "wss://api.meta.ai/v1/asr/realtime"
ASR_MODEL = "muse-voice-transcribe-1.0"
API_KEY_ENV = "STT_API_KEY"
ASR_RATE = 24_000
CHUNK_S = 0.1
TRAILING_SILENCE_S = 2.0
MAX_CLIP_SAMPLES = 30 * SAMPLE_RATE
STREAM_TIMEOUT_S = 60.0


@dataclass(frozen=True)
class CascadeResult:
    """One clip's transcript and its word error rate; a row of the per-sample JSONL."""

    id: str
    text: str
    transcript: str
    edits: int
    reference_words: int
    wer: float | None
    audio_seconds: float
    end_to_transcript_s: float | None


def to_pcm(clip: np.ndarray) -> bytes:
    """`clip`, mono float at 16 kHz, as little-endian 16-bit PCM at 24 kHz."""
    resampled = soxr.resample(clip, SAMPLE_RATE, ASR_RATE)
    return (np.clip(resampled, -1.0, 1.0) * 32767).astype("<i2").tobytes()


def read_stream(messages: Sequence[str]) -> tuple[str, bool]:
    """The transcript in the service's text `messages` so far, and whether the stream is done.

    Completed turns are joined with spaces. Before any turn completes, the latest
    partial stands in, so a cut-off stream still has its words.
    """
    turns: list[str] = []
    partial = ""
    done = False
    for message in messages:
        event = json.loads(message)
        kind = event.get("type")
        if kind == "speechComplete":
            turns.append(event.get("transcript", "").strip())
        elif kind == "transcript":
            partial = event.get("transcript", "").strip() or partial
            done = done or bool(event.get("final"))
        elif kind == "error":
            raise RuntimeError(f"recognizer error: {event.get('message', 'unknown')}")
    return (" ".join(t for t in turns if t) if turns else partial), done


async def transcribe(
    clip: np.ndarray, api_key: str, speed: float, url: str = ASR_URL
) -> tuple[str, float | None]:
    """The transcript of `clip` from the recognizer at `url`, and the latency from the end
    of the speech to the last completed turn, if any turn completed.
    """
    import websockets

    pcm = to_pcm(clip)
    chunk = int(ASR_RATE * CHUNK_S) * 2
    silence = bytes(int(ASR_RATE * TRAILING_SILENCE_S) * 2)
    init = {
        "authorization": {"accessToken": f"Bearer {api_key}"},
        "audioEncoding": "PCM_24KHZ",
        "model": ASR_MODEL,
        "mode": "ENDPOINTING",
    }
    messages: list[str] = []
    speech_sent: float | None = None
    last_turn: float | None = None
    async with websockets.connect(url) as socket:
        await socket.send(json.dumps(init))
        ack = json.loads(await asyncio.wait_for(socket.recv(), STREAM_TIMEOUT_S))
        if "sessionId" not in ack:
            raise RuntimeError(f"recognizer refused the session: {ack.get('message', ack)}")

        async def receive() -> None:
            nonlocal last_turn
            async for message in socket:
                if isinstance(message, str):
                    messages.append(message)
                    if '"speechComplete"' in message:
                        last_turn = time.perf_counter()
                    if read_stream(messages)[1]:
                        return

        receiver = asyncio.create_task(receive())
        data = pcm + silence
        for start in range(0, len(data), chunk):
            await socket.send(data[start : start + chunk])
            if start + chunk >= len(pcm) and speech_sent is None:
                speech_sent = time.perf_counter()
            await asyncio.sleep(CHUNK_S / speed)
        await socket.send(json.dumps({"type": "endStream"}))
        await asyncio.wait_for(receiver, STREAM_TIMEOUT_S)
    transcript, _ = read_stream(messages)
    latency = last_turn - speech_sent if last_turn and speech_sent else None
    return transcript, latency


def score(record_id: str, text: str, transcript: str, clip: np.ndarray, latency: float | None):
    """The `CascadeResult` of `transcript` against the reference `text`."""
    reference = normalize(text)
    edits = edit_distance(reference, normalize(transcript))
    return CascadeResult(
        id=record_id,
        text=text,
        transcript=transcript,
        edits=edits,
        reference_words=len(reference),
        wer=edits / len(reference) if reference else None,
        audio_seconds=len(clip) / SAMPLE_RATE,
        end_to_transcript_s=latency,
    )


def summarize(results: Sequence[CascadeResult]) -> dict[str, Any]:
    """Pooled word error rate over all reference words, and the median latency where measured."""
    words = sum(r.reference_words for r in results)
    latencies = [r.end_to_transcript_s for r in results if r.end_to_transcript_s is not None]
    return {
        "samples": len(results),
        "wer": sum(r.edits for r in results) / words if words else None,
        "empty_transcripts": sum(not r.transcript for r in results),
        "median_end_to_transcript_s": statistics.median(latencies) if latencies else None,
    }


async def run(
    ids: Sequence[str],
    texts: Sequence[str],
    clips: Sequence[np.ndarray],
    api_key: str,
    *,
    speed: float,
    concurrency: int,
) -> list[CascadeResult]:
    """Transcribes and scores every clip, `concurrency` connections at a time, in input order."""
    gate = asyncio.Semaphore(concurrency)

    async def one(i: int) -> CascadeResult:
        async with gate:
            transcript, latency = await transcribe(clips[i], api_key, speed)
        result = score(ids[i], texts[i], transcript, clips[i], latency)
        print(json.dumps({"id": result.id, "wer": result.wer}), flush=True)
        return result

    return list(await asyncio.gather(*(one(i) for i in range(len(clips)))))


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", type=Path, required=True, help="teacher records JSONL")
    parser.add_argument(
        "--audio-manifest",
        type=Path,
        action="append",
        required=True,
        help="data manifest that locates the records' audio; repeat for several",
    )
    parser.add_argument("--out", type=Path, required=True, help="per-sample results JSONL")
    parser.add_argument("--split", default="eval")
    parser.add_argument("--limit", type=int, help="the first N records of the split")
    parser.add_argument(
        "--speed", type=float, default=10.0, help="audio sent at this many times real time"
    )
    parser.add_argument("--concurrency", type=int, default=16)
    add_arguments(parser)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = _parse_args(argv)
    api_key = os.environ.get(API_KEY_ENV)
    if not api_key:
        raise SystemExit(f"set {API_KEY_ENV} to the Meta API key")
    records = read_records(args.manifest, args.split, args.limit)
    audio = resolve_audio(records, args.audio_manifest)
    augmenter = build_augmenter(args.noise_manifest, args.augment_prob, audio, AUGMENT_SEED)
    clips = [read_audio(path, MAX_CLIP_SAMPLES) for path in audio]
    if augmenter is not None:
        clips = [augmenter(clip, index) for index, clip in enumerate(clips)]
    results = asyncio.run(
        run(
            [r.id for r in records],
            [r.text for r in records],
            clips,
            api_key,
            speed=args.speed,
            concurrency=args.concurrency,
        )
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows = "".join(json.dumps(asdict(r), ensure_ascii=False) + "\n" for r in results)
    args.out.write_text(rows, encoding="utf-8")
    summary = summarize(results) | {
        "model": ASR_MODEL,
        "speed": args.speed,
        "augment": {"prob": args.augment_prob, "seed": AUGMENT_SEED},
    }
    text = json.dumps(summary, indent=2)
    args.out.with_suffix(SUMMARY_SUFFIX).write_text(text + "\n", encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
