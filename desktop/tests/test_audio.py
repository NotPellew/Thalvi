"""Exercises native audio framing, wake selection, and playback interruption without hardware."""

import asyncio
import sys
import threading
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

from thalvi_desktop.audio import Audio, samples, transcription_pcm
from thalvi_desktop.config import Config
from thalvi_desktop.cues import RATE, Cue, synthesize
from thalvi_desktop.wake import Wake


def test_microphone_frames_have_the_exact_formats_used_by_wake_and_recognizer():
    frame = np.full(3840, 1234, dtype="<i2").tobytes()
    assert np.array_equal(samples(frame, 3), np.full(1280, 1234, dtype="<i2"))
    assert transcription_pcm(frame) == np.full(1920, 1234, dtype="<i2").tobytes()
    with pytest.raises(RuntimeError, match="misaligned"):
        transcription_pcm(bytes(6))


def test_wake_routes_scores_and_validates_backend(monkeypatch, tmp_path):
    config = Config.load(Path(__file__).parents[1] / "config.example.json")
    for name in (
        "capture.onnx",
        "hey_mycroft_v0.1.onnx",
        "melspectrogram.onnx",
        "embedding_model.onnx",
    ):
        (tmp_path / name).touch()
    scores = {"capture": 0.0, "hey_mycroft_v0.1": 0.0}
    resets = []

    class Session:
        def get_inputs(self):
            return [SimpleNamespace(type="tensor(float)", shape=[1, 16, 96])]

        def get_outputs(self):
            return [SimpleNamespace(type="tensor(float)", shape=[1, 1])]

    class Model:
        def __init__(self, **kwargs):
            assert kwargs["wakeword_models"] == [
                str(tmp_path / "capture.onnx"),
                str(tmp_path / "hey_mycroft_v0.1.onnx"),
            ]
            assert kwargs["inference_framework"] == "onnx" and kwargs["ncpu"] == 1
            self.models = {key: Session() for key in scores}

        def predict(self, frame):
            assert frame.shape == (1280,) and frame.dtype == np.int16 and frame[0] == -32768
            return scores

        def reset(self):
            resets.append(True)

    monkeypatch.setitem(sys.modules, "openwakeword", SimpleNamespace(__path__=[]))
    monkeypatch.setitem(sys.modules, "openwakeword.model", SimpleNamespace(Model=Model))
    wake = Wake(replace(config, model_dir=tmp_path, capture_model=tmp_path / "capture.onnx"))
    frame = np.full(3840, -32768, dtype="<i2").tobytes()
    for a, b, expected in [
        (0, 0, None),
        (0.5, 0, "capture"),
        (0, 0.5, "conversation"),
        (0.5, 0.5, "ambiguous"),
    ]:
        scores["capture"], scores["hey_mycroft_v0.1"] = a, b
        assert wake.detect(frame) == expected
    for value in (float("nan"), float("inf"), -0.1, 1.1, None, [0.5], True):
        scores["capture"] = value
        with pytest.raises(RuntimeError, match="Wake detection failed"):
            wake.detect(frame)
    del scores["capture"]
    with pytest.raises(RuntimeError, match="Wake detection failed"):
        wake.detect(frame)
    assert not resets
    wake.reset()
    assert resets == [True]
    with pytest.raises(RuntimeError, match="Wake detection failed"):
        wake.detect(b"bad")


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


def test_input_epoch_rejects_callbacks_already_scheduled_before_resume(monkeypatch):
    streams = []

    class Stream:
        def __init__(self, **kwargs):
            self.callback = kwargs["callback"]
            streams.append(self)

        def start(self):
            pass

        def close(self):
            pass

    monkeypatch.setitem(sys.modules, "sounddevice", SimpleNamespace(RawInputStream=Stream))

    async def behavior():
        async with Audio() as audio:
            streams[0].callback(b"old", 1, None, None)
            audio.set_input(False)
            streams[0].callback(b"during cue", 1, None, None)
            audio.set_input(True)
            streams[0].callback(b"fresh", 1, None, None)
            assert await asyncio.wait_for(audio.next(), 1) == b"fresh"
            assert audio.frames.empty()

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "input_shape, output_shape, input_type, output_type, valid",
    [
        ([None, 16, 96], ["batch", 1], "tensor(float)", "tensor(float)", True),
        ([1, 16, 96], [1, 1], "tensor(float)", "tensor(float)", True),
        ([1, 0, 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, 15, 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, 17, 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, 120, 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, 121, 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, "time", 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([2, 16, 96], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, 16, 95], [1, 1], "tensor(float)", "tensor(float)", False),
        ([1, 16, 96], [1, 2], "tensor(float)", "tensor(float)", False),
        ([1, 16, 96], [1, 1], "tensor(double)", "tensor(float)", False),
        ([1, 16, 96], [1, 1], "tensor(float)", "tensor(int64)", False),
    ],
)
def test_wake_head_contract_on_first_detection_and_after_reset(
    monkeypatch, tmp_path, input_shape, output_shape, input_type, output_type, valid
):
    config = Config.load(Path(__file__).parents[1] / "config.example.json")
    for name in (
        "capture.onnx",
        "hey_mycroft_v0.1.onnx",
        "melspectrogram.onnx",
        "embedding_model.onnx",
    ):
        (tmp_path / name).touch()

    class Session:
        def get_inputs(self):
            return [SimpleNamespace(type=input_type, shape=input_shape)]

        def get_outputs(self):
            return [SimpleNamespace(type=output_type, shape=output_shape)]

    class Model:
        def __init__(self, **_kwargs):
            self.models = {key: Session() for key in ("capture", "hey_mycroft_v0.1")}
            self.reset()

        def predict(self, _frame):
            self.history += 1
            features = np.zeros((1, self.history, 96), dtype=np.float32)
            for session in self.models.values():
                context = session.get_inputs()[0].shape[1]
                assert features[:, -context:, :].shape == (1, context, 96)
            return {key: 0.0 for key in self.models}

        def reset(self):
            self.history = 41

    monkeypatch.setitem(sys.modules, "openwakeword", SimpleNamespace(__path__=[]))
    monkeypatch.setitem(sys.modules, "openwakeword.model", SimpleNamespace(Model=Model))
    config = replace(config, model_dir=tmp_path, capture_model=tmp_path / "capture.onnx")
    if valid:
        wake = Wake(config)
        frame = bytes(7680)
        assert wake.detect(frame) is None
        wake.reset()
        assert wake.detect(frame) is None
    else:
        with pytest.raises(RuntimeError, match="invalid metadata"):
            Wake(config)


def test_wake_missing_assets_and_backend_failure(monkeypatch, tmp_path):
    config = replace(
        Config.load(Path(__file__).parents[1] / "config.example.json"),
        model_dir=tmp_path,
        capture_model=tmp_path / "capture.onnx",
    )
    with pytest.raises(RuntimeError, match="assets are missing"):
        Wake(config)
    for name in (
        "capture.onnx",
        "hey_mycroft_v0.1.onnx",
        "melspectrogram.onnx",
        "embedding_model.onnx",
    ):
        (tmp_path / name).touch()

    class Model:
        def __init__(self, **_kwargs):
            raise OSError("backend failed")

    monkeypatch.setitem(sys.modules, "openwakeword", SimpleNamespace(__path__=[]))
    monkeypatch.setitem(sys.modules, "openwakeword.model", SimpleNamespace(Model=Model))
    with pytest.raises(RuntimeError, match="could not load"):
        Wake(config)


@pytest.mark.parametrize("prior", [True, False])
@pytest.mark.parametrize("outcome", ["ok", "fail", "cancel"])
def test_cue_playback_drops_microphone_frames_and_restores_prior_input(monkeypatch, outcome, prior):
    streams, played, stopped = [], [], []
    started, release = threading.Event(), threading.Event()

    class Stream:
        def __init__(self, **kwargs):
            self.callback = kwargs["callback"]
            streams.append(self)

        def start(self):
            pass

        def close(self):
            pass

    def play(tone, rate, *, blocking):
        played.append((tone, rate, blocking))
        started.set()
        assert release.wait(2)
        if outcome == "fail":
            raise OSError("no output device")

    monkeypatch.setitem(
        sys.modules,
        "sounddevice",
        SimpleNamespace(RawInputStream=Stream, play=play, stop=lambda: stopped.append(True)),
    )

    async def behavior():
        async with Audio() as audio:
            audio.set_input(prior)
            playing = asyncio.create_task(audio.play_cue(Cue.ACCEPTED))
            assert await asyncio.to_thread(started.wait, 1)
            streams[0].callback(b"during", 1, None, None)
            await asyncio.sleep(0)
            assert not audio.input_enabled and audio.frames.empty()
            streams[0].callback(b"late", 1, None, None)
            if outcome == "cancel":
                playing.cancel()
            release.set()
            if outcome == "ok":
                streams[0].callback(b"spanning", 1, None, None)
                await playing
            else:
                error = RuntimeError if outcome == "fail" else asyncio.CancelledError
                with pytest.raises(error):
                    await playing
            assert audio.input_enabled is prior
            streams[0].callback(b"fresh", 1, None, None)
            if prior:
                assert await asyncio.wait_for(audio.next(), 1) == b"fresh"
            assert audio.frames.empty()

    asyncio.run(behavior())
    tone, rate, blocking = played.pop()
    assert np.array_equal(tone, synthesize(Cue.ACCEPTED)) and rate == RATE and blocking
    assert stopped == ([True] if outcome == "cancel" else [])


def test_overlapping_cues_play_serially_and_keep_input_gated(monkeypatch):
    streams, plays = [], []
    started, release = [threading.Event() for _ in range(2)], [threading.Event() for _ in range(2)]

    class Stream:
        def __init__(self, **kwargs):
            self.callback = kwargs["callback"]
            streams.append(self)

        def start(self):
            pass

        def close(self):
            pass

    def play(tone, _rate, *, blocking):
        index = len(plays)
        plays.append(len(tone))
        started[index].set()
        assert release[index].wait(2)

    monkeypatch.setitem(
        sys.modules,
        "sounddevice",
        SimpleNamespace(RawInputStream=Stream, play=play, stop=lambda: None),
    )

    async def behavior():
        async with Audio() as audio:
            first = asyncio.create_task(audio.play_cue(Cue.CANCELLED))
            assert await asyncio.to_thread(started[0].wait, 1)
            second = asyncio.create_task(audio.play_cue(Cue.PROVISIONAL))
            await asyncio.sleep(0.05)
            assert plays == [len(synthesize(Cue.CANCELLED))] and not audio.input_enabled
            release[0].set()
            assert await asyncio.to_thread(started[1].wait, 1)
            assert not audio.input_enabled
            streams[0].callback(b"during second", 1, None, None)
            release[1].set()
            await asyncio.wait_for(asyncio.gather(first, second), 1)
            assert audio.input_enabled and audio.frames.empty()
            assert plays == [len(synthesize(Cue.CANCELLED)), len(synthesize(Cue.PROVISIONAL))]

    asyncio.run(behavior())
