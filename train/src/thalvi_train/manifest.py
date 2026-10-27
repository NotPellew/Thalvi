"""The training manifest: one JSONL row per audio sample, with strict reading and writing."""

import json
import re
from collections.abc import Iterable
from dataclasses import asdict, dataclass
from pathlib import Path, PurePosixPath
from typing import Literal, get_args

Task = Literal["answer", "continue"]
Split = Literal["train", "eval"]
Source = Literal["kokoro", "librispeech", "voxpopuli", "fleurs"]

_SAFE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")


class ManifestError(ValueError):
    """A manifest row or file breaks the format."""


@dataclass(frozen=True)
class Sample:
    """One utterance and the exact words spoken in it.

    `audio` is relative to the directory holding the manifest file, so a dataset
    directory can move as a whole. `task` says what Glimmer should do with the
    audio: `answer` a spoken request, or `continue` read speech.
    """

    id: str
    audio: str
    text: str
    task: Task
    split: Split
    voice: str
    source: Source

    def __post_init__(self) -> None:
        for name in ("id", "audio", "text", "task", "split", "voice", "source"):
            if not isinstance(getattr(self, name), str):
                raise ManifestError(f"{name} must be a string")
        if not _SAFE_ID.fullmatch(self.id):
            raise ManifestError(f"id {self.id!r} is not filesystem-safe")
        audio = PurePosixPath(self.audio)
        if not self.audio or audio.is_absolute() or "\\" in self.audio:
            raise ManifestError(f"audio {self.audio!r} must be a relative POSIX path")
        if not self.text.strip():
            raise ManifestError("text is empty")
        if not self.voice:
            raise ManifestError("voice is empty")
        for name, allowed in (("task", Task), ("split", Split), ("source", Source)):
            if getattr(self, name) not in get_args(allowed):
                raise ManifestError(f"{name} {getattr(self, name)!r} not in {get_args(allowed)}")


def write_manifest(path: Path, samples: Iterable[Sample]) -> int:
    """Writes `samples` as JSONL, rejecting duplicate ids, and returns the row count."""
    seen: set[str] = set()
    lines: list[str] = []
    for sample in samples:
        if sample.id in seen:
            raise ManifestError(f"duplicate id {sample.id!r}")
        seen.add(sample.id)
        lines.append(json.dumps(asdict(sample), ensure_ascii=False))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(f"{line}\n" for line in lines), encoding="utf-8")
    return len(lines)


def read_manifest(path: Path) -> list[Sample]:
    """Reads and validates a manifest; every failure names the file and line."""
    samples: list[Sample] = []
    seen: set[str] = set()
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            sample = _parse_row(line)
            if sample.id in seen:
                raise ManifestError(f"duplicate id {sample.id!r}")
        except ManifestError as error:
            raise ManifestError(f"{path}:{number}: {error}") from error
        seen.add(sample.id)
        samples.append(sample)
    return samples


def audio_path(manifest: Path, sample: Sample) -> Path:
    """The file `sample.audio` points at, resolved against the manifest's directory."""
    return manifest.parent / sample.audio


def _parse_row(line: str) -> Sample:
    try:
        row = json.loads(line)
    except json.JSONDecodeError as error:
        raise ManifestError(f"invalid JSON: {error}") from error
    if not isinstance(row, dict):
        raise ManifestError("row is not a JSON object")
    fields = set(Sample.__dataclass_fields__)
    if row.keys() != fields:
        missing, extra = sorted(fields - row.keys()), sorted(row.keys() - fields)
        raise ManifestError(f"fields differ: missing {missing}, unexpected {extra}")
    return Sample(**row)
