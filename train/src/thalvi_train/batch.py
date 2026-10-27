"""Batching: padded teacher and student sequences, and the positions that predict the response."""

from collections.abc import Callable, Sequence
from dataclasses import dataclass

import torch
import torch.nn.functional as F

from thalvi_train.glimmer import PAD_TOKEN_ID
from thalvi_train.prompts import Example, History, PromptTokenizer, build_example
from thalvi_train.records import Record


@dataclass(frozen=True)
class Sequences:
    """Right-padded ids with the `(row, col)` hidden positions whose logits predict the response."""

    input_ids: torch.Tensor
    attention_mask: torch.Tensor
    rows: torch.Tensor
    cols: torch.Tensor

    @property
    def tokens(self) -> int:
        return int(self.attention_mask.sum())


@dataclass(frozen=True)
class Batch:
    """A micro-batch: encoder frames, both prompts, and the response tokens to predict."""

    frames: torch.Tensor
    audio_tokens: list[int]
    teacher: Sequences
    student: Sequences
    targets: torch.Tensor


def pad_sequences(
    ids: Sequence[Sequence[int]], prompt_lens: Sequence[int], response_lens: Sequence[int]
) -> Sequences:
    """Pads `ids` on the right; position `prompt_len - 1 + j` predicts response token `j`."""
    width = max(len(row) for row in ids)
    input_ids = torch.tensor([[*row, *[PAD_TOKEN_ID] * (width - len(row))] for row in ids])
    mask = torch.tensor([[1] * len(row) + [0] * (width - len(row)) for row in ids])
    rows = torch.cat([torch.full((n,), i) for i, n in enumerate(response_lens)])
    cols = torch.cat(
        [torch.arange(p - 1, p - 1 + n) for p, n in zip(prompt_lens, response_lens, strict=True)]
    )
    return Sequences(input_ids, mask, rows, cols)


def pad_frames(frames: Sequence[torch.Tensor]) -> torch.Tensor:
    """Zero-pads `(T_i, dim)` frames on the right to `(batch, max T, dim)`."""
    longest = max(clip.shape[0] for clip in frames)
    return torch.stack([F.pad(clip, (0, 0, 0, longest - clip.shape[0])) for clip in frames])


def make_batch(
    records: Sequence[Record],
    frames: Sequence[torch.Tensor],
    tokenizer: PromptTokenizer,
    audio_token_count: Callable[[int], int],
    max_response_tokens: int | None = None,
    histories: Sequence[History] | None = None,
) -> Batch:
    """Builds the batch for `records` whose encoder frames are `frames`.

    `histories[i]` holds the earlier text turns in front of `records[i]`; none by default.
    """
    audio_tokens = [audio_token_count(clip.shape[0]) for clip in frames]
    examples: list[Example] = [
        build_example(record, tokenizer, count, max_response_tokens, history)
        for record, count, history in zip(
            records, audio_tokens, histories or [()] * len(records), strict=True
        )
    ]
    response_lens = [len(example.response_ids) for example in examples]
    sides = []
    for ids in ([e.teacher_ids for e in examples], [e.student_ids for e in examples]):
        prompt_lens = [len(row) - n for row, n in zip(ids, response_lens, strict=True)]
        sides.append(pad_sequences(ids, prompt_lens, response_lens))
    targets = torch.tensor([token for example in examples for token in example.response_ids])
    return Batch(pad_frames(frames), audio_tokens, sides[0], sides[1], targets)
