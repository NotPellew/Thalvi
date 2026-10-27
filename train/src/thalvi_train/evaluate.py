"""Evaluation CLI: how well Glimmer answers from audio, against the teacher's transcript-fed answer.

Run as `python -m thalvi_train.evaluate`.

Per held-out sample the student, Glimmer reading projected audio, decodes greedily
with a KV cache. Glimmer's reasoning opens by repeating the request, so that first
paragraph is the student's transcription of the audio and is scored as a word
error rate against the sample's text. The training KL on the teacher's stored
response is reported too. Times cover the decoder only: Whisper features are
read from the cache, so projection onward is timed, the encoder is not.
"""

import argparse
import json
import re
import statistics
import time
import unicodedata
from collections import defaultdict
from collections.abc import Callable, Sequence
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import torch
from safetensors.torch import load_file
from transformers import AutoTokenizer

from thalvi_train.batch import make_batch
from thalvi_train.features import FRAMES_PER_SECOND
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.projector import AudioProjector
from thalvi_train.prompts import (
    AUDIO_TOKEN_TEXT,
    CONTINUE_PREFIX,
    GENERATION_PROMPT,
    GlimmerTokenizer,
    PromptTokenizer,
    generation_ids,
)
from thalvi_train.records import Record, load_frames, read_records
from thalvi_train.teacher import EOT_ID
from thalvi_train.train import (
    PROJECTOR_CONFIG,
    PROJECTOR_WEIGHTS,
    ProjectorConfig,
    distill_loss,
)

SUMMARY_SUFFIX = ".summary.json"

_PARAGRAPH_BREAK = re.compile(r"\n\s*\n")
_REASONING = re.compile(r"to=self<\|message\|>(.*?)(?:<\|eom\|>|<\|eot\|>|\Z)", re.DOTALL)
_ANSWER = re.compile(r"to=user<\|message\|>(.*?)(?:<\|eot\|>|\Z)", re.DOTALL)


@dataclass(frozen=True)
class Generation:
    """Greedy output: the new ids (including a final stop id), and the seconds it took.

    `time_to_first_token_s` runs from the start of the prompt's prefill to the first
    token chosen; `seconds` to the last.
    """

    ids: list[int]
    finished: bool
    time_to_first_token_s: float
    seconds: float


@dataclass(frozen=True)
class Baseline:
    """The same Glimmer weights answering from the transcript text instead of audio."""

    response: str
    answer: str
    finished: bool
    generate_s: float


@dataclass(frozen=True)
class SampleResult:
    """One evaluated sample; a row of the per-sample JSONL.

    `wer` is `edits / reference_words` of `transcript` against the record's text, and
    `None` when the reference normalises to no words.
    """

    id: str
    task: str
    text: str
    kl: float
    transcript: str
    edits: int
    reference_words: int
    wer: float | None
    answer: str
    finished: bool
    new_tokens: int
    audio_seconds: float
    time_to_first_token_s: float
    generate_s: float
    response: str
    baseline: Baseline | None


def normalize(text: str) -> list[str]:
    """Words of `text` for scoring: lower-cased, dashes as spaces, other punctuation dropped."""
    spaced = "".join(" " if unicodedata.category(c) == "Pd" else c for c in text.lower())
    return "".join(c for c in spaced if c.isalnum() or c.isspace()).split()


def edit_distance(reference: Sequence[str], hypothesis: Sequence[str]) -> int:
    """Word-level Levenshtein distance: substitutions, insertions and deletions cost one."""
    previous = list(range(len(hypothesis) + 1))
    for i, ref_word in enumerate(reference, start=1):
        current = [i]
        for j, hyp_word in enumerate(hypothesis, start=1):
            current.append(
                min(
                    previous[j] + 1,
                    current[j - 1] + 1,
                    previous[j - 1] + (ref_word != hyp_word),
                )
            )
        previous = current
    return previous[-1]


def split_reply(generated: str) -> tuple[str, str]:
    """The reasoning and the answer in decoded generated text; either is empty if absent.

    Tolerates a generation cut off anywhere, since the student may run out of tokens.
    """
    reasoning = _REASONING.search(generated)
    answer = _ANSWER.search(generated)
    return (reasoning.group(1) if reasoning else "", answer.group(1) if answer else "")


def transcript_of(reasoning: str, task: str) -> str:
    """The reasoning's first paragraph, minus the `continue` instruction the request carries."""
    first = _PARAGRAPH_BREAK.split(reasoning.strip(), maxsplit=1)[0].strip()
    if task == "continue" and first.lower().startswith(CONTINUE_PREFIX.lower()):
        return first[len(CONTINUE_PREFIX) :].strip()
    return first


def _sync(device: torch.device) -> None:
    if device.type == "cuda":
        torch.cuda.synchronize(device)


@torch.no_grad()
def generate(
    glimmer: FrozenGlimmer, prompt_embeds: torch.Tensor, max_new_tokens: int, stop_id: int = EOT_ID
) -> Generation:
    """Greedy decoding from `prompt_embeds` `(1, length, hidden)` with a KV cache.

    Stops after emitting `stop_id`, or after `max_new_tokens` ids.
    """
    if max_new_tokens < 1:
        raise ValueError("max_new_tokens must be at least 1")
    device = glimmer.device
    _sync(device)
    started = time.perf_counter()
    hidden, cache = glimmer.cached_hidden_states(prompt_embeds)
    token = int(glimmer.logits(hidden[:, -1]).argmax())
    _sync(device)
    first_token = time.perf_counter() - started
    ids = [token]
    while token != stop_id and len(ids) < max_new_tokens:
        step = glimmer.embed_text(torch.tensor([[token]]))
        hidden, cache = glimmer.cached_hidden_states(step, cache)
        token = int(glimmer.logits(hidden[:, -1]).argmax())
        ids.append(token)
    _sync(device)
    return Generation(ids, token == stop_id, first_token, time.perf_counter() - started)


def _read(tokenizer: PromptTokenizer, generation: Generation) -> tuple[str, str, str]:
    """The decoded response, its reasoning and its answer."""
    response = GENERATION_PROMPT + tokenizer.decode(generation.ids)
    return (response, *split_reply(response))


def evaluate_record(
    glimmer: FrozenGlimmer,
    tokenizer: PromptTokenizer,
    projector: AudioProjector,
    record: Record,
    frames: torch.Tensor,
    *,
    max_new_tokens: int,
    with_text_baseline: bool,
) -> SampleResult:
    """Scores one record: its KL, and the student's greedy reply from its encoder `frames`."""
    device = glimmer.device
    autocast = torch.autocast(device.type, dtype=torch.bfloat16, enabled=device.type == "cuda")
    with torch.no_grad(), autocast:
        batch = make_batch([record], [frames], tokenizer, projector.audio_token_count)
        kl = distill_loss(glimmer, projector, batch).kl.item()
        count = batch.audio_tokens[0]
        audio = projector(frames.unsqueeze(0).to(device))[0, :count]
        prompt = generation_ids(record.task, AUDIO_TOKEN_TEXT * count, tokenizer)
        embeds = glimmer.embed_with_audio(torch.tensor([prompt]), [audio])
        student = generate(glimmer, embeds, max_new_tokens)
        baseline = None
        if with_text_baseline:
            text_prompt = generation_ids(record.task, record.text, tokenizer)
            teacher = generate(
                glimmer, glimmer.embed_text(torch.tensor([text_prompt])), max_new_tokens
            )
            response, _, answer = _read(tokenizer, teacher)
            baseline = Baseline(response, answer, teacher.finished, teacher.seconds)

    response, reasoning, answer = _read(tokenizer, student)
    transcript = transcript_of(reasoning, record.task)
    reference = normalize(record.text)
    edits = edit_distance(reference, normalize(transcript))
    return SampleResult(
        id=record.id,
        task=record.task,
        text=record.text,
        kl=kl,
        transcript=transcript,
        edits=edits,
        reference_words=len(reference),
        wer=edits / len(reference) if reference else None,
        answer=answer,
        finished=student.finished,
        new_tokens=len(student.ids),
        audio_seconds=frames.shape[0] / FRAMES_PER_SECOND,
        time_to_first_token_s=student.time_to_first_token_s,
        generate_s=student.seconds,
        response=response,
        baseline=baseline,
    )


def _group_summary(results: Sequence[SampleResult]) -> dict[str, Any]:
    words = sum(r.reference_words for r in results)
    return {
        "samples": len(results),
        "mean_kl": statistics.fmean(r.kl for r in results),
        "wer": sum(r.edits for r in results) / words if words else None,
        "finished_rate": sum(r.finished for r in results) / len(results),
    }


def summarize(results: Sequence[SampleResult]) -> dict[str, Any]:
    """Corpus figures over `results`, overall and per task.

    `wer` is total edits over total reference words. `baseline_finished_rate`
    appears only when every sample has a text-prompt baseline.
    """
    if not results:
        raise ValueError("no results to summarize")
    by_task: dict[str, list[SampleResult]] = defaultdict(list)
    for result in results:
        by_task[result.task].append(result)
    summary = _group_summary(results) | {
        "median_time_to_first_token_s": statistics.median(r.time_to_first_token_s for r in results),
        "by_task": {task: _group_summary(group) for task, group in sorted(by_task.items())},
    }
    baselines = [r.baseline for r in results if r.baseline]
    if len(baselines) == len(results):
        summary["baseline_finished_rate"] = sum(b.finished for b in baselines) / len(baselines)
    return summary


def evaluate(
    glimmer: FrozenGlimmer,
    tokenizer: PromptTokenizer,
    projector: AudioProjector,
    records: Sequence[Record],
    features_dir: Path,
    *,
    encoder_dim: int,
    max_new_tokens: int,
    with_text_baseline: bool,
    emit: Callable[[SampleResult], None],
) -> list[SampleResult]:
    """Evaluates `records` in order, passing each result to `emit` as it is done."""
    results = []
    for record in records:
        frames = load_frames(features_dir, record, encoder_dim)
        result = evaluate_record(
            glimmer,
            tokenizer,
            projector,
            record,
            frames,
            max_new_tokens=max_new_tokens,
            with_text_baseline=with_text_baseline,
        )
        emit(result)
        results.append(result)
    return results


def load_projector(
    run_dir: Path, text_dim: int, device: torch.device
) -> tuple[AudioProjector, ProjectorConfig]:
    """The trained projector of a run directory, on `device`; its output must be `text_dim` wide."""
    config = ProjectorConfig(**json.loads((run_dir / PROJECTOR_CONFIG).read_text()))
    if config.text_dim != text_dim:
        raise ValueError(f"projector text_dim {config.text_dim} != Glimmer hidden size {text_dim}")
    projector = AudioProjector(**asdict(config))
    projector.load_state_dict(load_file(run_dir / PROJECTOR_WEIGHTS))
    return projector.to(device).eval(), config


def _load_tokenizer(path: Path) -> PromptTokenizer:
    return GlimmerTokenizer(AutoTokenizer.from_pretrained(path))


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", type=Path, required=True, help="teacher records JSONL")
    parser.add_argument("--features-dir", type=Path, required=True)
    parser.add_argument("--glimmer", type=Path, required=True, help="Glimmer checkpoint")
    parser.add_argument("--projector", type=Path, required=True, help="run dir with the projector")
    parser.add_argument("--out", type=Path, required=True, help="per-sample results JSONL")
    parser.add_argument("--split", default="eval")
    parser.add_argument("--limit", type=int, help="evaluate only the first N records of the split")
    parser.add_argument("--device", choices=["cpu", "cuda"], default="cpu")
    parser.add_argument("--quantize", action="store_true", help="4-bit NF4 decoder; needs cuda")
    parser.add_argument("--max-new-tokens", type=int, default=384)
    parser.add_argument(
        "--with-text-baseline",
        action="store_true",
        help="also decode from the transcript text with the same weights",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = _parse_args(argv)
    if args.quantize and args.device != "cuda":
        raise SystemExit("--quantize needs --device cuda")
    records = read_records(args.manifest, args.split, args.limit)
    if not records:
        raise SystemExit(f"no {args.split!r} records in {args.manifest}")
    glimmer = load_glimmer(args.glimmer, device=args.device, quantize=args.quantize)
    tokenizer = _load_tokenizer(args.glimmer)
    projector, config = load_projector(args.projector, glimmer.hidden_size, glimmer.device)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", encoding="utf-8") as sink:

        def emit(result: SampleResult) -> None:
            sink.write(json.dumps(asdict(result), ensure_ascii=False) + "\n")
            sink.flush()
            print(
                json.dumps({"id": result.id, "kl": result.kl, "wer": result.wer}),
                flush=True,
            )

        results = evaluate(
            glimmer,
            tokenizer,
            projector,
            records,
            args.features_dir,
            encoder_dim=config.encoder_dim,
            max_new_tokens=args.max_new_tokens,
            with_text_baseline=args.with_text_baseline,
            emit=emit,
        )
    summary = json.dumps(summarize(results), indent=2)
    args.out.with_suffix(SUMMARY_SUFFIX).write_text(summary + "\n", encoding="utf-8")
    print(summary)


if __name__ == "__main__":
    main()
