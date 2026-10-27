"""Turns audio into Whisper encoder frames: reading clips, loading the frozen encoder, encoding.

Frames come at 50 per second as fp16 `(T, d_model)` tensors; the 30 s window's padding is cut off.
"""

from collections.abc import Sequence
from pathlib import Path

import numpy as np
import soundfile
import soxr
import torch
from transformers import WhisperConfig, WhisperFeatureExtractor, WhisperModel
from transformers.models.whisper.modeling_whisper import WhisperEncoder

DEFAULT_MODEL = "openai/whisper-large-v3-turbo"
SAMPLE_RATE = 16_000
SAMPLES_PER_FRAME = 320
FRAMES_PER_SECOND = SAMPLE_RATE // SAMPLES_PER_FRAME


class FeatureError(RuntimeError):
    """The encoder or a clip cannot be used."""


def frame_count(samples: int) -> int:
    """Encoder frames that cover `samples` of 16 kHz audio: `ceil(samples / 320)`.

    Whisper's log-mel runs at 100 frames per second, so `samples` yield
    `ceil(samples / 160)` real mel frames; the encoder's stride-2 convolution reads
    mel frames `2j - 1..2j + 1` for frame `j`, which holds real audio exactly when
    mel frame `2j` does, so `ceil(ceil(samples / 160) / 2) = ceil(samples / 320)`
    frames carry audio and the rest of the 30 s window is padding.
    """
    return -(-samples // SAMPLES_PER_FRAME)


def read_audio(path: Path, max_samples: int) -> np.ndarray:
    """Reads `path` as mono float32 at 16 kHz, rejecting clips longer than `max_samples`."""
    data, rate = soundfile.read(path, dtype="float32", always_2d=True)
    audio = data.mean(axis=1)
    if rate != SAMPLE_RATE:
        audio = soxr.resample(audio, rate, SAMPLE_RATE)
    if audio.size == 0:
        raise FeatureError(f"{path}: no audio")
    if audio.size > max_samples:
        raise FeatureError(
            f"{path}: {audio.size / SAMPLE_RATE:.1f} s is longer than "
            f"the encoder window of {max_samples / SAMPLE_RATE:g} s"
        )
    return audio


def load_encoder(
    model: str, device: str, encoder_dim: int
) -> tuple[WhisperEncoder, WhisperFeatureExtractor]:
    """Loads `model`'s frozen encoder and feature extractor, fp16 on cuda and fp32 on cpu.

    The width is checked from the config before any weight is fetched, so a
    mismatch with the projector fails at once. Weights come through `WhisperModel`
    because `WhisperEncoder.from_pretrained` finds none of the checkpoint's
    `model.encoder.*` keys and silently keeps random ones.
    """
    config = WhisperConfig.from_pretrained(model)
    if config.d_model != encoder_dim:
        raise FeatureError(
            f"{model} has encoder width {config.d_model}, but the projector expects {encoder_dim}"
        )
    dtype = torch.float16 if device == "cuda" else torch.float32
    encoder = WhisperModel.from_pretrained(model, dtype=dtype).encoder
    encoder = encoder.to(device).eval().requires_grad_(False)
    return encoder, WhisperFeatureExtractor.from_pretrained(model)


@torch.inference_mode()
def encode(
    encoder: WhisperEncoder, extractor: WhisperFeatureExtractor, clips: Sequence[np.ndarray]
) -> list[torch.Tensor]:
    """Encodes 16 kHz clips as fp16 `(frame_count(len(clip)), d_model)` tensors on the CPU.

    Every clip is padded to the extractor's window; only frames carrying audio are kept.
    """
    mel = extractor(
        list(clips),
        sampling_rate=SAMPLE_RATE,
        padding="max_length",
        return_tensors="pt",
        device=encoder.device.type,
    ).input_features
    hidden = encoder(mel.to(device=encoder.device, dtype=encoder.dtype)).last_hidden_state
    frames = []
    for clip, row in zip(clips, hidden, strict=True):
        wanted = frame_count(len(clip))
        if wanted > row.shape[0]:
            raise FeatureError(f"{wanted} frames wanted, but the encoder emitted {row.shape[0]}")
        kept = row[:wanted].to(device="cpu", dtype=torch.float16)
        if not torch.isfinite(kept).all():
            raise FeatureError("encoder output is not finite; fp16 overflowed")
        frames.append(kept)
    return frames
