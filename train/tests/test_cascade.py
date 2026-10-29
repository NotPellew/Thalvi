import asyncio
import json
from pathlib import Path

import numpy as np
import pytest
from tiny_whisper import write_audio_manifest
from websockets.asyncio.server import serve

from thalvi_train import cascade
from thalvi_train.cascade import (
    ASR_RATE,
    TRAILING_SILENCE_S,
    RecognizerError,
    read_stream,
    to_pcm,
    transcribe,
    transcribe_paced,
)
from thalvi_train.features import SAMPLE_RATE
from thalvi_train.records import Record

RESPONSE = (
    "<|start|>assistant to=self<|message|>r<|eom|><|start|>assistant to=user<|message|>a<|eot|>"
)


def event(**fields: object) -> str:
    return json.dumps(fields)


@pytest.mark.parametrize(
    ("messages", "transcript", "done"),
    [
        ([], "", False),
        ([event(type="transcript", transcript="add", final=False)], "add", False),
        (
            [
                event(type="transcript", transcript="add milk", final=False),
                event(type="speechComplete", transcript="Add milk."),
                event(type="speechComplete", transcript=" And eggs. "),
                event(type="transcript", transcript="", final=True),
            ],
            "Add milk. And eggs.",
            True,
        ),
        ([event(type="speechStart"), event(type="audioProgress")], "", False),
    ],
)
def test_the_transcript_is_every_completed_turn_or_else_the_latest_partial(
    messages: list[str], transcript: str, done: bool
) -> None:
    assert read_stream(messages) == (transcript, done)


def test_a_recognizer_error_is_raised() -> None:
    with pytest.raises(RecognizerError, match="boom"):
        read_stream([event(type="error", message="boom")])


def test_a_clip_the_recognizer_falls_behind_on_is_resent_slower_down_to_real_time(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    speeds: list[float] = []

    async def behind(clip: np.ndarray, api_key: str, speed: float):
        speeds.append(speed)
        if speed > 1:
            raise RecognizerError("recognizer error: Audio processing backlog too large")
        return "ok", None

    monkeypatch.setattr(cascade, "transcribe", behind)
    clip = np.zeros(1, dtype=np.float32)

    assert asyncio.run(transcribe_paced(clip, "k", 5.0)) == ("ok", None)
    assert speeds == [5.0, 2.5, 1.25, 1.0]


def test_other_recognizer_errors_are_not_retried(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[float] = []

    async def broken(clip: np.ndarray, api_key: str, speed: float):
        calls.append(speed)
        raise RecognizerError("recognizer error: bad key")

    monkeypatch.setattr(cascade, "transcribe", broken)
    with pytest.raises(RecognizerError, match="bad key"):
        asyncio.run(transcribe_paced(np.zeros(1, dtype=np.float32), "k", 4.0))
    assert calls == [4.0]


def test_pcm_is_16_bit_at_24_khz_and_clipped() -> None:
    pcm = to_pcm(np.full(SAMPLE_RATE, 2.0, dtype=np.float32))
    samples = np.frombuffer(pcm, dtype="<i2")
    assert len(samples) == ASR_RATE
    assert samples[ASR_RATE // 2] == 32767


def test_transcribe_streams_the_clip_and_silence_then_reads_the_turns() -> None:
    """A local stand-in for the recognizer: it acks, counts the audio, and answers `endStream`."""
    received: dict[str, object] = {"bytes": 0}

    async def recognizer(socket) -> None:
        received["init"] = json.loads(await socket.recv())
        await socket.send(event(sessionId="s"))
        async for message in socket:
            if isinstance(message, bytes):
                received["bytes"] += len(message)
            elif json.loads(message)["type"] == "endStream":
                await socket.send(event(type="speechComplete", transcript="hello there"))
                await socket.send(event(type="transcript", transcript="", final=True))

    async def scenario() -> tuple[str, float | None]:
        async with serve(recognizer, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]
            clip = np.zeros(SAMPLE_RATE // 2, dtype=np.float32)
            return await transcribe(clip, "key", speed=1000.0, url=f"ws://127.0.0.1:{port}")

    transcript, latency = asyncio.run(scenario())

    assert transcript == "hello there"
    assert latency is not None and latency >= 0
    assert received["init"]["authorization"] == {"accessToken": "Bearer key"}
    assert received["bytes"] == (ASR_RATE // 2 + int(ASR_RATE * TRAILING_SILENCE_S)) * 2


def test_cli_scores_each_eval_clip_and_writes_a_summary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    records = [
        Record("a", "what is two plus two", "answer", "eval", RESPONSE),
        Record("b", "once upon a time", "continue", "eval", RESPONSE),
        Record("c", "ignored train row", "answer", "train", RESPONSE),
    ]
    manifest = tmp_path / "teacher.jsonl"
    manifest.write_text(
        "".join(json.dumps(r.__dict__ | {"source": "x"}) + "\n" for r in records),
        encoding="utf-8",
    )
    audio = write_audio_manifest(tmp_path / "speech", {"a": 1600, "b": 1600, "c": 1600})
    heard = {1600: ["what is two plus two", "once upon a"]}

    async def fake_transcribe(clip: np.ndarray, api_key: str, speed: float):
        assert api_key == "secret"
        return heard[len(clip)].pop(0), 0.5

    monkeypatch.setattr(cascade, "transcribe", fake_transcribe)
    monkeypatch.setenv(cascade.API_KEY_ENV, "secret")
    out = tmp_path / "cascade.jsonl"

    cascade.main(["--manifest", str(manifest), "--audio-manifest", str(audio), "--out", str(out)])

    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert [(r["id"], r["edits"]) for r in rows] == [("a", 0), ("b", 1)]
    summary = json.loads(out.with_suffix(".summary.json").read_text(encoding="utf-8"))
    assert summary["wer"] == pytest.approx(1 / 9)
    assert summary["median_end_to_transcript_s"] == 0.5
    assert summary["empty_transcripts"] == 0


def test_cli_needs_the_api_key(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(cascade.API_KEY_ENV, raising=False)
    with pytest.raises(SystemExit, match="STT_API_KEY"):
        cascade.main(
            ["--manifest", "m", "--audio-manifest", "a", "--out", str(tmp_path / "o.jsonl")]
        )
