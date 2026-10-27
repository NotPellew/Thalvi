"""A tiny random Whisper checkpoint and a speech manifest of noise clips for CPU tests."""

from pathlib import Path

import numpy as np
import soundfile
import torch
from transformers import WhisperConfig, WhisperFeatureExtractor, WhisperModel

from thalvi_train.manifest import Sample, write_manifest

DIM = 16
WINDOW = 32_000


def save_tiny_whisper(path: Path) -> None:
    """Saves a two-layer encoder whose window is `WINDOW` samples, 100 frames, `DIM` wide."""
    config = WhisperConfig(
        d_model=DIM,
        encoder_layers=2,
        encoder_attention_heads=2,
        encoder_ffn_dim=64,
        decoder_layers=1,
        decoder_attention_heads=2,
        decoder_ffn_dim=64,
        num_mel_bins=16,
        max_source_positions=WINDOW // 320,
        vocab_size=64,
        pad_token_id=1,
        bos_token_id=1,
        eos_token_id=2,
        decoder_start_token_id=3,
    )
    torch.manual_seed(0)
    WhisperModel(config).save_pretrained(path)
    WhisperFeatureExtractor(feature_size=16, chunk_length=WINDOW // 16_000).save_pretrained(path)


def tone(samples: int, seed: int = 0) -> np.ndarray:
    return np.random.default_rng(seed).standard_normal(samples).astype(np.float32) * 0.1


def write_audio_manifest(root: Path, lengths: dict[str, int]) -> Path:
    """Writes a 16 kHz wav of `lengths[id]` samples per id under `root/audio` and its manifest."""
    (root / "audio").mkdir(parents=True, exist_ok=True)
    samples = []
    for seed, (name, length) in enumerate(lengths.items()):
        soundfile.write(root / "audio" / f"{name}.wav", tone(length, seed), 16_000)
        samples.append(
            Sample(name, f"audio/{name}.wav", "hello", "answer", "train", "af_x", "kokoro")
        )
    manifest = root / "manifest.jsonl"
    write_manifest(manifest, samples)
    return manifest
