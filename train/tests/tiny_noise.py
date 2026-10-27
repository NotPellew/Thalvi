"""A tiny noise manifest of generated background clips for CPU tests."""

import json
from pathlib import Path

import soundfile
from tiny_whisper import tone

from thalvi_train.augment import Category


def write_noise_manifest(root: Path, clips: dict[str, tuple[Category, int]]) -> Path:
    """Writes a 16 kHz wav of `clips[name][1]` samples per name under `root` and its manifest."""
    root.mkdir(parents=True, exist_ok=True)
    rows = []
    for seed, (name, (category, length)) in enumerate(clips.items(), start=100):
        soundfile.write(root / f"{name}.wav", tone(length, seed), 16_000)
        rows.append({"path": f"{name}.wav", "category": category, "seconds": length / 16_000})
    manifest = root / "noise.jsonl"
    manifest.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
    return manifest
