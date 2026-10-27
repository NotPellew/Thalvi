"""Distillation records: the manifest fields training reads, and the audio each one is spoken in."""

import json
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, get_args

from thalvi_train.manifest import ManifestError, audio_path, read_manifest

Task = Literal["answer", "continue"]

RESPONSE_START = "<|start|>assistant to=self"
RESPONSE_END = "<|eot|>"


@dataclass(frozen=True)
class Record:
    """One training sample.

    `response` is the teacher's whole assistant turn as rendered text, from
    `RESPONSE_START` through the final `RESPONSE_END`.
    """

    id: str
    text: str
    task: Task
    split: str
    response: str

    def __post_init__(self) -> None:
        if self.task not in get_args(Task):
            raise ValueError(f"{self.id}: unknown task {self.task!r}")
        if not (self.response.startswith(RESPONSE_START) and self.response.endswith(RESPONSE_END)):
            raise ValueError(
                f"{self.id}: response must run from {RESPONSE_START!r} to {RESPONSE_END!r}"
            )


def read_records(manifest: Path, split: str, limit: int | None = None) -> list[Record]:
    """Reads the first `limit` records of `split` from a manifest JSONL, ignoring other fields."""
    records: list[Record] = []
    with manifest.open() as lines:
        for line in lines:
            if limit is not None and len(records) >= limit:
                break
            fields = json.loads(line)
            if fields["split"] != split:
                continue
            records.append(
                Record(
                    id=fields["id"],
                    text=fields["text"],
                    task=fields["task"],
                    split=fields["split"],
                    response=fields["response"],
                )
            )
    return records


def resolve_audio(records: Sequence[Record], manifests: Sequence[Path]) -> list[Path]:
    """The audio file of each record, found by id in the data `manifests`, in record order.

    Records carry no audio path; the data manifests do. An id in two manifests, or a
    record in none, is an error naming the id.
    """
    paths: dict[str, Path] = {}
    for manifest in manifests:
        for sample in read_manifest(manifest):
            if sample.id in paths:
                raise ManifestError(
                    f"{manifest}: id {sample.id!r} is already in an earlier manifest"
                )
            paths[sample.id] = audio_path(manifest, sample)
    missing = next((r.id for r in records if r.id not in paths), None)
    if missing is not None:
        raise ManifestError(f"{missing}: no audio for this record in {[str(m) for m in manifests]}")
    return [paths[record.id] for record in records]
