"""Distillation records: the manifest fields training reads, and their cached encoder features."""

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, get_args

import torch
from safetensors.torch import load_file

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


def load_frames(features_dir: Path, record: Record, encoder_dim: int) -> torch.Tensor:
    """Loads `<features_dir>/<id>.safetensors`'s `frames` as float32 `(frames, encoder_dim)`."""
    frames = load_file(features_dir / f"{record.id}.safetensors")["frames"]
    if frames.ndim != 2 or frames.shape[1] != encoder_dim:
        raise ValueError(f"{record.id}: frames {tuple(frames.shape)} is not (T, {encoder_dim})")
    return frames.float()
