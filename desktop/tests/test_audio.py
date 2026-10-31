"""Exercises native audio framing, wake selection, and playback interruption without hardware."""

import asyncio
import sys
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

from thalvi_desktop.audio import Audio, samples, transcription_pcm
from thalvi_desktop.config import Config
from thalvi_desktop.wake import Wake


def test_microphone_frames_have_the_exact_formats_used_by_wake_and_recognizer():
    frame = np.full(3840, 1234, dtype="<i2").tobytes()
    assert np.array_equal(samples(frame, 3), np.full(1280, 1234, dtype="<i2"))
    assert transcription_pcm(frame) == np.full(1920, 1234, dtype="<i2").tobytes()
    with pytest.raises(RuntimeError, match="misaligned"):
        transcription_pcm(bytes(6))


def test_wake_uses_explicit_existing_models_without_downloads(monkeypatch, tmp_path):
    config = Config.load(Path(__file__).parents[1] / "config.example.json")
    paths = [
        tmp_path / name
        for name in (
            "capture.onnx",
            "conversation.onnx",
            "embedding_model.onnx",
            "melspectrogram.onnx",
        )
    ]
    for path in paths:
        path.touch()
    predictions = {"capture": 0.1, "conversation": 0.1}
    calls = []

    class Model:
        def __init__(self, **kwargs):
            calls.append(kwargs)

        def predict(self, frame):
            assert frame.shape == (1280,)
            assert frame.dtype == np.dtype("int16")
            return predictions

        def reset(self):
            calls.append("reset")

    monkeypatch.setitem(sys.modules, "openwakeword.model", SimpleNamespace(Model=Model))
    wake = Wake(
        replace(config, capture_model=paths[0], conversation_model=paths[1], model_dir=tmp_path)
    )
    frame = bytes(7680)
    assert wake.detect(frame) is None
    predictions["capture"] = 0.8
    assert wake.detect(frame) == "capture"
    predictions["conversation"] = 0.9
    assert wake.detect(frame) == "conversation"
    assert calls[0]["inference_framework"] == "onnx"
    assert calls[0]["melspec_model_path"] == str(paths[3])
    wake.reset()
    assert calls[-1] == "reset"


def test_overflow_stops_capture_and_playback_stop_discards_all_queued_audio(monkeypatch):
    streams = []

    class Stream:
        active = True

        def __init__(self, **kwargs):
            self.callback = kwargs["callback"]
            streams.append(self)

        def start(self):
            pass

        def close(self):
            self.active = False

    monkeypatch.setitem(
        sys.modules, "sounddevice", SimpleNamespace(RawInputStream=Stream, RawOutputStream=Stream)
    )

    async def behavior():
        async with Audio() as audio:
            for _ in range(126):
                streams[0].callback(bytes(7680), 3840, None, "")
            with pytest.raises(RuntimeError, match="audio was lost"):
                await asyncio.wait_for(audio.next(), 1)
            audio.play(b"\x01\x02" * 1000)
            out = bytearray(960)
            streams[1].callback(out, 480, None, None)
            assert out == b"\x01\x02" * 480
            audio.stop()
            streams[1].callback(out, 480, None, None)
            assert out == bytes(960)
            assert audio.drained_bytes() == 2000
            audio.new_conversation()
            assert audio.drained_bytes() == 0
        assert all(not stream.active for stream in streams)

    asyncio.run(behavior())
