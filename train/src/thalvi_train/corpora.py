"""Turns VoxPopuli and FLEURS parquet files into FLAC clips with manifests, and lists MUSAN.

The speech corpora become `Sample` manifests of 16 kHz mono FLAC clips of at most
`MAX_SECONDS`; MUSAN, which is only mixed into other audio, becomes a
`NoiseClip` manifest, which `augment.read_noise_manifest` reads. Downloading and
packing into tars for Modal live in `modal_app`.

Source columns, as found in the Hugging Face parquet files:

- VoxPopuli: `audio_id`, `audio` (`{bytes, path}`, WAV), `raw_text` (cased and
  punctuated, empty for some rows), `normalized_text` (lower-case), `speaker_id`
  (the string `"None"` when unknown), `accent` (`en_xx` in `en_accented`).
- FLEURS: `path` (a unique `<digits>.wav`; `id` repeats, once per speaker reading a
  sentence), `audio` (`{bytes, path}`, WAV), `raw_transcription`, `gender` (an
  integer class, 0 male, 1 female, 2 other). There is no speaker id.
"""

import hashlib
import io
import json
import re
import tarfile
from collections import Counter
from collections.abc import Callable, Container, Iterable, Iterator
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Literal, get_args

import numpy as np
import pyarrow.parquet as pq
import soundfile
import soxr

from thalvi_train.librispeech import MAX_SECONDS, sentence_case
from thalvi_train.manifest import ManifestError, Sample, Source, Split, write_manifest

SAMPLE_RATE = 16_000
BATCH_ROWS = 64
EVAL_FRACTION = 0.15
UNKNOWN_VOICE = "unknown"
NO_SPEAKER = "None"
MANIFEST_NAME = "manifest.jsonl"

NoiseCategory = Literal["noise", "music", "speech"]
FLEURS_GENDERS = ("male", "female", "other")

_UNSAFE_ID_CHARS = re.compile(r"[^A-Za-z0-9._-]")


@dataclass(frozen=True)
class Clip:
    """One source row before conversion: WAV (or any libsndfile) bytes and what is said in them.

    An empty `text` marks a row with no transcript, which `write_clips` drops.
    """

    id: str
    audio: bytes
    text: str
    voice: str
    split: Split


@dataclass
class Written:
    """What `write_clips` produced: the samples with their seconds, and what it left out."""

    samples: list[Sample] = field(default_factory=list)
    seconds: dict[str, float] = field(default_factory=dict)
    too_long: int = 0
    no_text: int = 0
    no_audio: int = 0


@dataclass(frozen=True)
class NoiseClip:
    """One MUSAN file. `path` is relative to the directory holding the manifest."""

    path: str
    category: NoiseCategory
    seconds: float

    def __post_init__(self) -> None:
        if not self.path or self.path.startswith("/") or "\\" in self.path:
            raise ManifestError(f"path {self.path!r} must be a relative POSIX path")
        if self.category not in get_args(NoiseCategory):
            raise ManifestError(f"category {self.category!r} not in {get_args(NoiseCategory)}")
        if not self.seconds > 0:
            raise ManifestError(f"seconds {self.seconds!r} must be positive")


def safe_id(prefix: str, name: str) -> str:
    """`prefix` and `name` joined, with every character a manifest id forbids turned into `-`."""
    return prefix + _UNSAFE_ID_CHARS.sub("-", name)


def decode_clip(data: bytes) -> tuple[np.ndarray, int]:
    """Reads audio bytes as mono float32 and returns it with its sample rate."""
    audio, rate = soundfile.read(io.BytesIO(data), dtype="float32", always_2d=True)
    return audio.mean(axis=1), rate


def write_clips(clips: Iterable[Clip], root: Path, source: Source) -> Written:
    """Writes each clip as 16 kHz mono FLAC under `root/audio` and its manifest to `root`.

    Clips without text, with empty or undecodable audio (some VoxPopuli rows have
    none), and over `MAX_SECONDS` are left out and counted. A repeated id raises at
    once, before more audio is converted.
    """
    audio_dir = root / "audio"
    audio_dir.mkdir(parents=True, exist_ok=True)
    written = Written()
    for clip in clips:
        if clip.id in written.seconds:
            raise ManifestError(f"duplicate id {clip.id!r}")
        if not clip.text.strip():
            written.no_text += 1
            continue
        try:
            audio, rate = decode_clip(clip.audio)
        except soundfile.LibsndfileError:
            audio, rate = np.empty(0, dtype=np.float32), SAMPLE_RATE
        if audio.size == 0:
            print(f"{clip.id}: no usable audio, left out", flush=True)
            written.no_audio += 1
            continue
        if rate != SAMPLE_RATE:
            audio = soxr.resample(audio, rate, SAMPLE_RATE)
        seconds = audio.size / SAMPLE_RATE
        if seconds > MAX_SECONDS:
            written.too_long += 1
            continue
        relative = f"audio/{clip.id}.flac"
        soundfile.write(root / relative, audio.clip(-1.0, 1.0), SAMPLE_RATE, subtype="PCM_16")
        written.seconds[clip.id] = seconds
        written.samples.append(
            Sample(clip.id, relative, clip.text.strip(), "continue", clip.split, clip.voice, source)
        )
    write_manifest(root / MANIFEST_NAME, written.samples)
    return written


def rows(path: Path, columns: list[str]) -> Iterator[dict[str, Any]]:
    """The `columns` of a parquet file as dicts, read a batch at a time."""
    for batch in pq.ParquetFile(path).iter_batches(batch_size=BATCH_ROWS, columns=columns):
        yield from batch.to_pylist()


def speaker_counts(path: Path) -> Counter[str]:
    """Clips per speaker id in a VoxPopuli parquet file, reading only that column."""
    counts: Counter[str] = Counter()
    for row in rows(path, ["speaker_id"]):
        counts[row["speaker_id"]] += 1
    return counts


def choose_eval_speakers(counts: dict[str, int], clips: float) -> set[str]:
    """Whole speakers, taken in a fixed hash order, until they hold at least `clips` clips.

    Choosing speakers rather than clips keeps eval voices out of training; going by
    clips rather than speakers keeps the eval share near the wanted one when a few
    speakers have most of the clips. The order depends only on the ids.
    """
    chosen: set[str] = set()
    taken = 0
    for speaker in sorted(counts, key=lambda s: hashlib.sha256(s.encode()).hexdigest()):
        if taken >= clips:
            break
        chosen.add(speaker)
        taken += counts[speaker]
    return chosen


@dataclass(frozen=True)
class VoxPopuliPlan:
    """Which VoxPopuli speakers are eval, so that no eval speaker has training clips.

    `accented_eval` are the held-out speakers of the accented file; `test_speakers`
    are the speakers of the English test file that stay eval.
    """

    accented_eval: frozenset[str]
    test_speakers: frozenset[str]
    test_clips_left_out: int


def plan_voxpopuli(
    train: Counter[str],
    accented: Counter[str],
    test: Counter[str],
    fraction: float = EVAL_FRACTION,
) -> VoxPopuliPlan:
    """Splits speakers of the English training, accented and English test files.

    The corpus reuses speaker ids across its files, so held-out accented speakers are
    taken only from those without clips in the English training file, up to `fraction` of
    the accented clips. The English test file keeps only speakers that have no clips in
    any training data, since an official test split can still share speakers with train;
    its clips of speakers who are unknown or in training are left out and counted.
    """
    free = {s: n for s, n in accented.items() if s not in train and s != NO_SPEAKER}
    accented_eval = choose_eval_speakers(free, fraction * sum(accented.values()))
    training = (set(train) | set(accented)) - accented_eval - {NO_SPEAKER}
    kept = {s for s in test if s not in training and s != NO_SPEAKER}
    return VoxPopuliPlan(
        frozenset(accented_eval),
        frozenset(kept),
        sum(test.values()) - sum(test[s] for s in kept),
    )


def voxpopuli_clips(
    path: Path,
    split_of: Callable[[str], Split],
    speakers: Container[str] | None = None,
) -> Iterator[Clip]:
    """Clips of a VoxPopuli parquet file, each in the split `split_of` gives its speaker id.

    Only rows of `speakers` are read when given. The text is `raw_text`; a row whose
    raw text is empty falls back to the normalized text in sentence case. The voice is
    the speaker id, `unknown` when the corpus has none.
    """
    columns = ["audio_id", "audio", "raw_text", "normalized_text", "speaker_id"]
    for row in rows(path, columns):
        speaker = row["speaker_id"]
        if speakers is not None and speaker not in speakers:
            continue
        text = row["raw_text"].strip() or sentence_case(row["normalized_text"].strip())
        yield Clip(
            id=safe_id("vp-", row["audio_id"]),
            audio=row["audio"]["bytes"],
            text=text,
            voice=UNKNOWN_VOICE if speaker in ("", NO_SPEAKER) else speaker,
            split=split_of(speaker),
        )


def fleurs_clips(path: Path, language: str, split: Split) -> Iterator[Clip]:
    """Clips of a FLEURS parquet file of `language` (`en` or `de`), all in `split`.

    The id comes from the audio file's name, which is unique; the voice is the
    speaker's gender, since the corpus has no speaker ids.
    """
    columns = ["path", "audio", "raw_transcription", "gender"]
    for row in rows(path, columns):
        yield Clip(
            id=safe_id(f"fl-{language}-", Path(row["path"]).stem),
            audio=row["audio"]["bytes"],
            text=row["raw_transcription"],
            voice=FLEURS_GENDERS[row["gender"]],
            split=split,
        )


def summary_lines(
    name: str, written: Written, label: Callable[[Sample], str] = lambda sample: sample.split
) -> list[str]:
    """Clip, hour and voice counts per `label` group, and what was left out, for the log."""
    groups: dict[str, list[Sample]] = {}
    for sample in written.samples:
        groups.setdefault(label(sample), []).append(sample)
    lines = []
    for group, samples in sorted(groups.items()):
        hours = sum(written.seconds[sample.id] for sample in samples) / 3600
        voices = len({sample.voice for sample in samples})
        lines.append(f"{name} {group}: {len(samples)} clips, {hours:.2f} h, {voices} voices")
    lines.append(
        f"{name}: {written.too_long} clips over {MAX_SECONDS:g} s, "
        f"{written.no_text} without text and {written.no_audio} without audio left out"
    )
    return lines


def shared_speakers(written: Written) -> set[str]:
    """Known speaker ids that have clips in both the train and the eval split."""
    by_split = {
        split: {s.voice for s in written.samples if s.split == split and s.voice != UNKNOWN_VOICE}
        for split in get_args(Split)
    }
    return by_split["train"] & by_split["eval"]


def list_musan(root: Path) -> list[NoiseClip]:
    """Every WAV under `root/noise`, `root/music` and `root/speech`, with its length in seconds."""
    clips = []
    for category in get_args(NoiseCategory):
        for wav in sorted((root / category).rglob("*.wav")):
            clips.append(
                NoiseClip(
                    wav.relative_to(root).as_posix(),
                    category,
                    round(soundfile.info(wav).duration, 3),
                )
            )
    return clips


def write_noise_manifest(path: Path, clips: Iterable[NoiseClip]) -> int:
    """Writes `clips` as JSONL, one `{path, category, seconds}` row each, and returns the count."""
    lines = [json.dumps(asdict(clip)) for clip in clips]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
    return len(lines)


def pack_tar(root: Path, archive: Path) -> None:
    """Packs `root` into the uncompressed `archive`, members named `<root name>/...`.

    The manifest goes first, so a reader that only wants it can stop after one
    member. The archive appears under its name only once complete.
    """
    partial = archive.with_name(archive.name + ".part")
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(partial, "w") as tar:
        tar.add(root / MANIFEST_NAME, arcname=f"{root.name}/{MANIFEST_NAME}")
        for path in sorted(root.rglob("*")):
            if path.is_file() and path != root / MANIFEST_NAME:
                tar.add(path, arcname=f"{root.name}/{path.relative_to(root).as_posix()}")
    partial.replace(archive)
