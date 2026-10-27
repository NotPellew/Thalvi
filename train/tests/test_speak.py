import asyncio
import io
import json
from collections.abc import Callable
from pathlib import Path

import httpx
import numpy as np
import pytest
import soundfile

from thalvi_train import speak as speak_module
from thalvi_train.manifest import read_manifest
from thalvi_train.questions import Question
from thalvi_train.speak import (
    SpeakError,
    check_wav,
    english_voices,
    held_out_voices,
    plan_for,
    speak,
)

BASE = "http://kokoro.test"
VOICE_IDS = [f"{prefix}{i:02d}" for prefix in ("af_", "am_", "bf_", "ef_", "zf_") for i in range(6)]


def wav_bytes(seconds: float = 0.5, rate: int = 24000) -> bytes:
    buffer = io.BytesIO()
    soundfile.write(buffer, np.zeros(int(seconds * rate)), rate, format="WAV")
    return buffer.getvalue()


class Kokoro:
    def __init__(self, speech: Callable[[httpx.Request], httpx.Response] | None = None) -> None:
        self.speech = speech or (lambda _request: httpx.Response(200, content=wav_bytes()))
        self.speech_requests: list[dict[str, object]] = []

    def transport(self) -> httpx.MockTransport:
        def respond(request: httpx.Request) -> httpx.Response:
            if request.url.path == "/v1/audio/voices":
                return httpx.Response(200, json={"voices": [{"id": v} for v in VOICE_IDS]})
            self.speech_requests.append(json.loads(request.content))
            return self.speech(request)

        return httpx.MockTransport(respond)

    def run(self, texts: list[str], out: Path, concurrency: int = 2) -> tuple[int, list[str]]:
        async def no_sleep(_seconds: float) -> None:
            return None

        return asyncio.run(
            speak(
                [Question(text=t, topic="general") for t in texts],
                out,
                base_url=BASE,
                concurrency=concurrency,
                transport=self.transport(),
                sleep=no_sleep,
            )
        )


def test_only_english_voices_are_used_and_every_fifth_is_held_out() -> None:
    voices = english_voices(VOICE_IDS)
    assert voices == sorted(v for v in VOICE_IDS if v[0] in "ab")
    assert held_out_voices(voices) == {voices[0], voices[5], voices[10], voices[15]}


def test_plan_is_stable_in_range_and_splits_by_voice() -> None:
    voices = english_voices(VOICE_IDS)
    plans = [plan_for(f"request {i}", voices) for i in range(300)]
    assert plans == [plan_for(f"request {i}", voices) for i in range(300)]
    assert all(0.9 <= p.speed <= 1.15 for p in plans)
    assert len({p.id for p in plans}) == 300
    assert {p.voice for p in plans} == set(voices)
    assert all((p.split == "eval") == (p.voice in held_out_voices(voices)) for p in plans)
    assert {p.split for p in plans} == {"train", "eval"}


@pytest.mark.parametrize("body", [b"", b"<html>error</html>", wav_bytes(0)])
def test_check_wav_rejects_non_audio(body: bytes) -> None:
    with pytest.raises(SpeakError):
        check_wav(body)


def test_check_wav_returns_the_duration() -> None:
    assert check_wav(wav_bytes(0.5)) == pytest.approx(0.5)


def test_writes_audio_and_a_manifest_of_answer_samples(tmp_path: Path) -> None:
    kokoro = Kokoro()
    texts = [
        "set a timer for ten minutes",
        "what's the capital of France",
        "set a timer for ten minutes",
    ]
    count, failed = kokoro.run(texts, tmp_path)
    samples = read_manifest(tmp_path / "manifest.jsonl")
    assert (count, failed, len(samples)) == (2, [], 2)
    assert all(s.task == "answer" and s.source == "kokoro" for s in samples)
    assert all(soundfile.info(tmp_path / s.audio).samplerate == 24000 for s in samples)
    request = kokoro.speech_requests[0]
    assert request["model"] == "kokoro"
    assert request["response_format"] == "wav"
    assert request["voice"] in english_voices(VOICE_IDS)
    assert 0.9 <= request["speed"] <= 1.15


def test_rerun_skips_existing_audio_and_keeps_the_manifest(tmp_path: Path) -> None:
    texts = ["one request", "another request"]
    first = Kokoro()
    first.run(texts, tmp_path)
    manifest = (tmp_path / "manifest.jsonl").read_text()
    second = Kokoro()
    second.run([*texts, "a third request"], tmp_path)
    assert len(second.speech_requests) == 1
    assert (tmp_path / "manifest.jsonl").read_text().startswith(manifest)


def test_a_non_audio_reply_is_requested_again(tmp_path: Path) -> None:
    replies = iter([httpx.Response(200, content=b""), httpx.Response(200, content=wav_bytes())])
    kokoro = Kokoro(lambda _request: next(replies))
    count, failed = kokoro.run(["only request"], tmp_path)
    assert (count, failed, len(kokoro.speech_requests)) == (1, [], 2)


def test_failed_requests_are_left_out_of_audio_and_manifest(tmp_path: Path) -> None:
    kokoro = Kokoro(lambda _request: httpx.Response(200, content=b"nope"))
    count, failed = kokoro.run(["only request"], tmp_path)
    assert (count, len(failed), len(kokoro.speech_requests)) == (0, 1, speak_module.AUDIO_ATTEMPTS)
    assert list((tmp_path / "audio").iterdir()) == []
    assert read_manifest(tmp_path / "manifest.jsonl") == []
