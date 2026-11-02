"""Writes wake_reference.json: per-frame scores from Python openwakeword 0.6.0.

Run from the desktop venv, with a directory holding the five model files and the test clip:
    desktop/.venv/bin/python -I generate_wake_reference.py MODEL_DIR hey_mycroft_test.wav
The seed noise is replaced by the same formula wake.rs uses, so both start from identical history.
"""

import hashlib
import json
import sys
import wave
from pathlib import Path

import numpy as np
import onnxruntime
import openwakeword
from openwakeword.model import Model

FRAME = 1280
HEADS = ("hey_thalvi_v0.0.13", "hey_mycroft_v0.1", "hey_jarvis_v0.1")


def lcg(seed):
    state = seed
    while True:
        state = (state * 1_664_525 + 1_013_904_223) % 2**32
        yield state >> 16


seed = lcg(1)
SEED_NOISE = np.array([next(seed) % 2000 - 1000 for _ in range(64_000)])
np.random.randint = lambda *_args, **_kwargs: SEED_NOISE.copy()


def formula_signal(frames):
    noise = lcg(7)
    samples = []
    for n in range(frames * FRAME):
        saw = (n * 61) % 4001 - 2000
        envelope = abs(n % 32_000 - 16_000)
        samples.append(saw // 2 + (next(noise) % 4001 - 2000) * envelope // 16_000)
    return np.array(samples, dtype=np.int16)


def clip_signal(path):
    with wave.open(str(path)) as clip:
        data = np.frombuffer(clip.readframes(clip.getnframes()), dtype=np.int16)
    silence = np.zeros(16_000, dtype=np.int16)
    padded = np.concatenate((silence, data, silence))
    return padded[: len(padded) // FRAME * FRAME]


def score(model_dir, signal, reset_before):
    model = Model(
        wakeword_models=[str(model_dir / f"{head}.onnx") for head in HEADS],
        inference_framework="onnx",
        melspec_model_path=str(model_dir / "melspectrogram.onnx"),
        embedding_model_path=str(model_dir / "embedding_model.onnx"),
        ncpu=1,
        vad_threshold=0,
        enable_speex_noise_suppression=False,
    )
    scores = {head: [] for head in HEADS}
    for index in range(len(signal) // FRAME):
        if index in reset_before:
            model.reset()
        predictions = model.predict(signal[index * FRAME : (index + 1) * FRAME])
        for head in HEADS:
            scores[head].append(float(predictions[head]))
    return {"reset_before": sorted(reset_before), "scores": scores}


def main():
    model_dir, clip = Path(sys.argv[1]), Path(sys.argv[2])
    fixture = {
        "openwakeword": "0.6.0",
        "onnxruntime": onnxruntime.__version__,
        "clip_sha256": hashlib.sha256(clip.read_bytes()).hexdigest(),
        "formula": score(model_dir, formula_signal(80), {40}),
        "clip": score(model_dir, clip_signal(clip), set()),
    }
    out = Path(__file__).with_name("wake_reference.json")
    out.write_text(json.dumps(fixture, indent=None, separators=(",", ":")) + "\n")


main()
