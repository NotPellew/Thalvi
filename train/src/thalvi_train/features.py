"""Caches frozen Whisper encoder features for a manifest; run as `python -m thalvi_train.features`.

Each sample's audio becomes `<features_dir>/<id>.safetensors` holding `frames`, a fp16
`(T, d_model)` tensor at 50 frames per second, the layout `records.load_frames` reads.
`features.json` beside them names the encoder, so a directory never mixes models.
"""

import argparse
import json
import os
import sys
import time
from collections.abc import Callable, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import asdict, dataclass
from pathlib import Path

import numpy as np
import soundfile
import soxr
import torch
from safetensors.torch import save_file
from transformers import WhisperConfig, WhisperFeatureExtractor, WhisperModel
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.manifest import Sample, audio_path, read_manifest
from thalvi_train.train import ProjectorConfig

DEFAULT_MODEL = "openai/whisper-large-v3-turbo"
ENCODER_DIM = ProjectorConfig.encoder_dim
SAMPLE_RATE = 16_000
SAMPLES_PER_FRAME = 320
FRAMES_PER_SECOND = SAMPLE_RATE // SAMPLES_PER_FRAME
INFO_NAME = "features.json"
IO_THREADS = 32
SUFFIX = ".safetensors"

Log = Callable[[dict[str, object]], None]


class FeatureError(RuntimeError):
    """The encoder, a clip, or the features directory cannot be used."""


@dataclass(frozen=True)
class FeaturesInfo:
    """What a features directory holds, saved as `features.json`."""

    model: str
    encoder_dim: int
    frames_per_second: int = FRAMES_PER_SECOND
    dtype: str = "float16"


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
    model: str, device: str, encoder_dim: int = ENCODER_DIM
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


def _finished(features_dir: Path) -> set[str]:
    """Names of finished feature files, from one directory listing.

    A file only gets its final name by an atomic rename after it is fully written,
    so its presence is proof enough; opening each one costs ~0.2 s on a Modal volume.
    """
    return {entry.name for entry in os.scandir(features_dir) if entry.name.endswith(SUFFIX)}


def _write_atomically(path: Path, frames: torch.Tensor) -> None:
    partial = path.with_name(path.name + ".part")
    save_file({"frames": frames.contiguous()}, partial)
    os.replace(partial, path)


def _claim_directory(features_dir: Path, info: FeaturesInfo) -> None:
    features_dir.mkdir(parents=True, exist_ok=True)
    path = features_dir / INFO_NAME
    if path.exists():
        existing = json.loads(path.read_text(encoding="utf-8"))
        if existing != asdict(info):
            raise FeatureError(f"{features_dir} holds {existing}, not {asdict(info)}")
        return
    partial = path.with_name(path.name + ".part")
    partial.write_text(json.dumps(asdict(info)) + "\n", encoding="utf-8")
    os.replace(partial, path)


def extract(
    manifest: Path,
    features_dir: Path,
    encoder: WhisperEncoder,
    extractor: WhisperFeatureExtractor,
    *,
    model: str,
    batch_size: int = 16,
    limit: int | None = None,
    log: Log = lambda line: None,
) -> tuple[int, int]:
    """Writes features for the first `limit` samples of `manifest` and returns `(written, skipped)`.

    A sample whose file already loads is skipped, so an interrupted run resumes. Each
    batch logs one line with the running count and clips per second. Reads and writes
    run on `IO_THREADS` threads, overlapping the encoder: on a Modal volume each
    file costs ~0.2 s of latency, which otherwise caps a run at ~5 clips/s.
    """
    info = FeaturesInfo(model=model, encoder_dim=encoder.config.d_model)
    samples = read_manifest(manifest)[:limit]
    _claim_directory(features_dir, info)
    with ThreadPoolExecutor(IO_THREADS) as pool:
        finished = _finished(features_dir)
        todo = [s for s in samples if _target(features_dir, s).name not in finished]
        skipped = len(samples) - len(todo)
        batches = [todo[i : i + batch_size] for i in range(0, len(todo), batch_size)]

        def read(batch: list[Sample]) -> list[Future[np.ndarray]]:
            return [
                pool.submit(read_audio, audio_path(manifest, s), extractor.n_samples) for s in batch
            ]

        reads = read(batches[0]) if batches else []
        writes: list[Future[None]] = []
        started = time.perf_counter()
        done = 0
        for index, batch in enumerate(batches):
            clips = [future.result() for future in reads]
            if index + 1 < len(batches):
                reads = read(batches[index + 1])
            frames = encode(encoder, extractor, clips)
            for future in writes:
                future.result()
            writes = [
                pool.submit(_write_atomically, _target(features_dir, sample), clip_frames)
                for sample, clip_frames in zip(batch, frames, strict=True)
            ]
            done += len(batch)
            log(
                {
                    "done": done,
                    "total": len(todo),
                    "skipped": skipped,
                    "clips_per_s": round(done / (time.perf_counter() - started), 3),
                }
            )
        for future in writes:
            future.result()
    return len(todo), skipped


def _target(features_dir: Path, sample: Sample) -> Path:
    return features_dir / f"{sample.id}{SUFFIX}"


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("features_dir", type=Path)
    parser.add_argument("--batch-size", type=int, default=16)
    parser.add_argument("--device", choices=["cpu", "cuda"])
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--limit", type=int)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    device = args.device or ("cuda" if torch.cuda.is_available() else "cpu")
    try:
        encoder, extractor = load_encoder(args.model, device)
        written, skipped = extract(
            args.manifest,
            args.features_dir,
            encoder,
            extractor,
            model=args.model,
            batch_size=args.batch_size,
            limit=args.limit,
            log=lambda line: print(json.dumps(line), flush=True),
        )
    except FeatureError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"written": written, "skipped": skipped}), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
