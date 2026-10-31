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


def test_wake_routes_keywords_and_resets(monkeypatch, tmp_path):
    config = Config.load(Path(__file__).parents[1] / "config.example.json")
    stem = "epoch-12-avg-2-chunk-16-left-64.int8.onnx"
    for name in ("bpe.model", *(f"{part}-{stem}" for part in ("encoder", "decoder", "joiner"))):
        (tmp_path / name).touch()
    (tmp_path / "tokens.txt").write_text("HEY 1\nTALVI 2\nMYCROFT 3\n")
    result = [""]
    streams = []

    class Tokenizer:
        def __init__(self, **kwargs):
            pass

        def encode(self, phrase, out_type):
            return phrase.split()

    class Stream:
        def accept_waveform(self, rate, frame):
            assert rate == 16000
            assert frame.shape == (1280,)
            assert frame.dtype == np.float32
            assert frame[0] == -1

    class Model:
        def __init__(self, **kwargs):
            assert (
                Path(kwargs["keywords_file"]).read_text()
                == "HEY TALVI @capture\nHEY MYCROFT @conversation\n"
            )
            self.keyword_spotter = self
            self.ready = False

        def create_stream(self):
            stream = Stream()
            streams.append(stream)
            self.ready = True
            return stream

        def is_ready(self, stream):
            return self.ready

        def decode_stream(self, stream):
            self.ready = False

        def get_result(self, stream):
            return SimpleNamespace(keyword=result[0])

    monkeypatch.setitem(
        sys.modules, "sentencepiece", SimpleNamespace(SentencePieceProcessor=Tokenizer)
    )
    monkeypatch.setitem(sys.modules, "sherpa_onnx", SimpleNamespace(KeywordSpotter=Model))
    wake = Wake(replace(config, model_dir=tmp_path))
    frame = np.full(3840, -32768, dtype="<i2").tobytes()
    assert wake.detect(frame) is None
    for keyword, expected in (
        ("capture", "capture"),
        ("conversation", "conversation"),
        ("other", None),
    ):
        wake.reset()
        result[0] = keyword
        assert wake.detect(frame) == expected
    with pytest.raises(RuntimeError, match="Wake detection failed"):
        wake.detect(b"bad")
    assert len(streams) == 7


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
