"""On-the-fly audio augmentation for training and evaluation: background, room reverb, gain.

Augmenting the audio leaves the transcript alone, so the teacher's stored answers stay
valid and only the student's input gets harder. The chain, applied to a 16 kHz clip with
probability `prob`, is:

1. a background at a random SNR: noise or music from the noise manifest (SNR 0 to 20 dB),
   or babble, 1 to 3 other speech clips (SNR 5 to 20 dB each, at random offsets);
2. with probability `REVERB_PROB`, a synthetic room impulse response (RT60 0.15 to 0.8 s);
3. a gain of -6 to +6 dB, clipped to [-1, 1].

Everything a clip gets is drawn from the run seed, the epoch and the sample index alone,
so a rerun augments identically. The noise manifest is a JSONL of
`{"path": <relative to the manifest>, "category": "noise"|"music"|"speech", "seconds": <float>}`
rows, read and checked once at startup. Decoded background clips are kept in a
size-bounded LRU cache shared by the read-ahead threads.
"""

import argparse
import json
import math
import random
import threading
from collections import OrderedDict
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, get_args

import numpy as np

from thalvi_train.features import SAMPLE_RATE, decode_audio

Category = Literal["noise", "music", "speech"]
BackgroundKind = Literal["noise", "music", "babble"]

NOISE_SNR_DB = (0.0, 20.0)
BABBLE_SNR_DB = (5.0, 20.0)
BABBLE_VOICES = (1, 3)
REVERB_PROB = 0.3
RT60_S = (0.15, 0.8)
GAIN_DB = (-6.0, 6.0)
TAIL_TO_DIRECT_ENERGY = 0.5
CACHE_SAMPLES = 32_000_000
SILENCE_RMS = 1e-6
_DECAY_60_DB = math.log(1000.0)


class NoiseManifestError(ValueError):
    """The noise manifest is unreadable or has a row that cannot be used."""


@dataclass(frozen=True)
class NoiseClip:
    """One background clip of the noise manifest, with its path resolved against the manifest."""

    path: Path
    category: Category
    seconds: float


def read_noise_manifest(manifest: Path) -> list[NoiseClip]:
    """The clips of a noise manifest JSONL, every row checked, every file present.

    Raises `NoiseManifestError` naming the line for a row that is not JSON, lacks a
    field, has an unknown category or a non-positive length, points outside the manifest's
    directory, or names a missing file; and for a manifest with no rows.
    """
    clips: list[NoiseClip] = []
    root = manifest.parent
    with manifest.open(encoding="utf-8") as lines:
        for number, line in enumerate(lines, start=1):
            if not line.strip():
                continue
            where = f"{manifest}:{number}"
            try:
                row = json.loads(line)
            except json.JSONDecodeError as error:
                raise NoiseManifestError(f"{where}: not JSON: {error}") from error
            if not isinstance(row, dict):
                raise NoiseManifestError(f"{where}: a row must be an object")
            relative, category, seconds = row.get("path"), row.get("category"), row.get("seconds")
            if not isinstance(relative, str) or not relative:
                raise NoiseManifestError(f"{where}: path must be a non-empty string")
            parts = Path(relative)
            if parts.is_absolute() or ".." in parts.parts:
                raise NoiseManifestError(f"{where}: path {relative!r} must stay inside {root}")
            if category not in get_args(Category):
                raise NoiseManifestError(f"{where}: unknown category {category!r}")
            if (
                isinstance(seconds, bool)
                or not isinstance(seconds, int | float)
                or not math.isfinite(seconds)
                or seconds <= 0
            ):
                raise NoiseManifestError(f"{where}: seconds must be a positive number")
            path = root / parts
            if not path.is_file():
                raise NoiseManifestError(f"{where}: {path} does not exist")
            clips.append(NoiseClip(path, category, float(seconds)))
    if not clips:
        raise NoiseManifestError(f"{manifest}: no clips")
    return clips


def rms(audio: np.ndarray) -> float:
    """Root mean square of `audio`."""
    return float(np.sqrt(np.mean(np.square(audio, dtype=np.float64))))


def fit_length(clip: np.ndarray, samples: int, start: int) -> np.ndarray:
    """`samples` of `clip` read circularly from `start`: a crop if it is longer, a loop if not."""
    return clip[(start + np.arange(samples)) % len(clip)]


def scaled_to_snr(speech: np.ndarray, background: np.ndarray, snr_db: float) -> np.ndarray:
    """`background` scaled so `speech` is `snr_db` louder than it by RMS; zeros if either is silent.

    The two must have the same length. Adding the result to `speech` mixes at that SNR;
    a silent clip has no level to be relative to, so it gets no background.
    """
    speech_rms, background_rms = rms(speech), rms(background)
    if speech_rms < SILENCE_RMS or background_rms < SILENCE_RMS:
        return np.zeros_like(speech)
    return background * np.float32(speech_rms / (background_rms * 10 ** (snr_db / 20)))


def room_impulse_response(rt60: float, rng: np.random.Generator) -> np.ndarray:
    """A synthetic room: a unit direct impulse, then noise decaying by 60 dB over `rt60` seconds.

    The tail carries `TAIL_TO_DIRECT_ENERGY` of the direct impulse's energy.
    """
    length = max(2, int(rt60 * SAMPLE_RATE))
    decay = np.exp(-_DECAY_60_DB * np.arange(length) / (rt60 * SAMPLE_RATE))
    tail = rng.standard_normal(length) * decay
    tail[0] = 0.0
    tail *= math.sqrt(TAIL_TO_DIRECT_ENERGY / np.sum(np.square(tail)))
    tail[0] = 1.0
    return tail


def add_reverb(audio: np.ndarray, impulse_response: np.ndarray) -> np.ndarray:
    """`audio` convolved with `impulse_response` by FFT, cut to its length, at its RMS."""
    size = 1 << (len(audio) + len(impulse_response) - 2).bit_length()
    spectrum = np.fft.rfft(audio, size) * np.fft.rfft(impulse_response, size)
    wet = np.fft.irfft(spectrum, size)[: len(audio)]
    level = rms(wet)
    if level < SILENCE_RMS:
        return audio
    return (wet * (rms(audio) / level)).astype(np.float32)


class ClipCache:
    """Decoded clips by path, least recently used out first, holding at most `max_samples`.

    Safe across threads. A clip larger than the whole budget is decoded but not kept;
    two threads missing the same path may both decode it.
    """

    def __init__(self, max_samples: int) -> None:
        self._max_samples = max_samples
        self._clips: OrderedDict[Path, np.ndarray] = OrderedDict()
        self._samples = 0
        self._lock = threading.Lock()

    def get(self, path: Path) -> np.ndarray:
        """The decoded 16 kHz mono clip at `path`, read-only."""
        with self._lock:
            if path in self._clips:
                self._clips.move_to_end(path)
                return self._clips[path]
        clip = decode_audio(path)
        clip.flags.writeable = False
        if clip.size <= self._max_samples:
            with self._lock:
                if path not in self._clips:
                    self._clips[path] = clip
                    self._samples += clip.size
                while self._samples > self._max_samples:
                    _, evicted = self._clips.popitem(last=False)
                    self._samples -= evicted.size
        return clip


class Augmenter:
    """Applies the augmentation chain to training or evaluation clips, reproducibly.

    `audio[i]` is the file of sample `i`: babble draws its voices from the noise
    manifest's `speech` clips and from the other samples' files, never sample `i`'s own.
    With `prob` 0 every clip passes through untouched.
    """

    def __init__(
        self,
        noise: Sequence[NoiseClip],
        audio: Sequence[Path],
        *,
        prob: float,
        seed: int,
        cache_samples: int = CACHE_SAMPLES,
    ) -> None:
        if not 0 <= prob <= 1:
            raise ValueError("the augmentation probability must be between 0 and 1")
        self.prob = prob
        self.seed = seed
        self._by_kind: dict[BackgroundKind, list[Path]] = {"noise": [], "music": []}
        voices: list[Path] = []
        for clip in noise:
            if clip.category == "speech":
                voices.append(clip.path)
            else:
                self._by_kind[clip.category].append(clip.path)
        self._voices = [*voices, *audio]
        self._own_offset = len(voices)
        kinds: list[BackgroundKind] = [kind for kind, paths in self._by_kind.items() if paths]
        if len(self._voices) >= 2:
            kinds.append("babble")
        if prob > 0 and not kinds:
            raise ValueError(
                "nothing to mix in: no noise or music, and fewer than two speech clips"
            )
        self._kinds = kinds
        self._cache = ClipCache(cache_samples)

    def __call__(self, clip: np.ndarray, index: int, epoch: int = 0) -> np.ndarray:
        """Sample `index`'s `clip` in `epoch`, augmented or not; same length, within [-1, 1].

        Seeded by the run seed, `epoch` and `index` alone.
        """
        rng = random.Random(f"{self.seed}:{epoch}:{index}")
        if rng.random() >= self.prob:
            return clip
        mixed = clip + self._background(clip, index, rng)
        if rng.random() < REVERB_PROB:
            room = room_impulse_response(
                rng.uniform(*RT60_S), np.random.default_rng(rng.getrandbits(64))
            )
            mixed = add_reverb(mixed, room)
        mixed = mixed * np.float32(10 ** (rng.uniform(*GAIN_DB) / 20))
        return np.clip(mixed, -1.0, 1.0, dtype=np.float32)

    def _background(self, clip: np.ndarray, index: int, rng: random.Random) -> np.ndarray:
        kind = rng.choice(self._kinds)
        if kind == "babble":
            return self._babble(clip, index, rng)
        background = self._cache.get(rng.choice(self._by_kind[kind]))
        fitted = fit_length(background, len(clip), rng.randrange(len(background)))
        return scaled_to_snr(clip, fitted, rng.uniform(*NOISE_SNR_DB))

    def _babble(self, clip: np.ndarray, index: int, rng: random.Random) -> np.ndarray:
        own = self._own_offset + index
        count = min(rng.randint(*BABBLE_VOICES), len(self._voices) - 1)
        mix = np.zeros_like(clip)
        for position in rng.sample(range(len(self._voices) - 1), count):
            voice = self._cache.get(self._voices[position + (position >= own)])
            fitted = fit_length(voice, len(clip), rng.randrange(len(voice)))
            mix += scaled_to_snr(clip, fitted, rng.uniform(*BABBLE_SNR_DB))
        return mix


def add_arguments(parser: argparse.ArgumentParser) -> None:
    """Adds `--noise-manifest` and `--augment-prob` to a CLI."""
    parser.add_argument(
        "--noise-manifest",
        type=Path,
        help="JSONL of background clips (noise, music, speech) to mix into the audio",
    )
    parser.add_argument(
        "--augment-prob",
        type=float,
        default=0.0,
        help="chance that a clip gets a background, room reverb and gain; 0 turns it off",
    )


def build_augmenter(
    noise_manifest: Path | None, prob: float, audio: Sequence[Path], seed: int
) -> Augmenter | None:
    """The `Augmenter` for a run, or `None` when `prob` is 0.

    The noise manifest is read and checked even then, so a bad one fails at startup.
    """
    noise = read_noise_manifest(noise_manifest) if noise_manifest is not None else []
    if prob == 0:
        return None
    return Augmenter(noise, audio, prob=prob, seed=seed)
