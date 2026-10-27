"""Evaluation CLI: how well Glimmer answers from audio, against the teacher's transcript-fed answer.

Run as `python -m thalvi_train.evaluate`.

Per held-out sample the student, Glimmer reading projected audio, decodes greedily
with a KV cache. Glimmer's reasoning opens by repeating the request, so that first
paragraph is the student's transcription of the audio and is scored as a word
error rate against the sample's text. The training KL on the teacher's stored
response is reported too.

Samples run in batches of similar prompt length, left-padded, and rows come back in
record order. The audio goes through the Whisper encoder inline, as in the live path,
a batch at a time. With `--history-prob`, samples run after earlier text turns drawn
from other evaluated records (see `history`), the same for every run of the same
records and settings; the summary states the setting. With `--augment-prob`, the clips
get backgrounds, reverb and gain under a fixed seed (see `augment`), for a noisy
evaluation that reruns identically; the summary states those settings too.

A batch's timings sit on each of its rows, since a batch has no
per-sample latency: `batch_encode_s` is the encoder, mel features included (reading
files is not timed), `batch_prefill_s` the decoder's prefill to the first token, and
`batch_generate_s` the whole decode, prefill included. The latency a single request sees
is measured apart, by `--latency-samples` unbatched runs of the same code.
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

import numpy as np
import torch
from safetensors.torch import load_file
from transformers import AutoTokenizer, WhisperFeatureExtractor
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.augment import Augmenter, add_arguments, build_augmenter
from thalvi_train.batch import Batch, make_batch
from thalvi_train.decode import decode_steps
from thalvi_train.features import (
    DEFAULT_MODEL,
    FRAMES_PER_SECOND,
    encode,
    frame_count,
    load_encoder,
    read_audio,
)
from thalvi_train.glimmer import PAD_TOKEN_ID, FrozenGlimmer, load_glimmer
from thalvi_train.history import HistoryConfig, HistorySampler, answer_of
from thalvi_train.projector import AudioProjector
from thalvi_train.prompts import (
    AUDIO_TOKEN_TEXT,
    CONTINUE_PREFIX,
    GENERATION_PROMPT,
    GlimmerTokenizer,
    History,
    PromptTokenizer,
    generation_ids,
)
from thalvi_train.records import Record, read_records, resolve_audio
from thalvi_train.teacher import EOT_ID
from thalvi_train.train import (
    PROJECTOR_WEIGHTS,
    ProjectorConfig,
    read_projector_config,
    token_losses,
)

SUMMARY_SUFFIX = ".summary.json"
HISTORY_SEED = 0
AUGMENT_SEED = 0

_PARAGRAPH_BREAK = re.compile(r"\n\s*\n")
_REASONING = re.compile(r"to=self<\|message\|>(.*?)(?:<\|eom\|>|<\|eot\|>|\Z)", re.DOTALL)


@dataclass(frozen=True)
class Generation:
    """Greedy output of a batch: per row the new ids (including a final stop id), and the seconds.

    `prefill_s` runs from the start of the prompts' prefill to the first tokens chosen;
    `seconds` to the last step.
    """

    ids: list[list[int]]
    finished: list[bool]
    prefill_s: float
    seconds: float


@dataclass(frozen=True)
class Baseline:
    """The same Glimmer weights answering from the transcript text instead of audio."""

    response: str
    answer: str
    finished: bool


@dataclass(frozen=True)
class SampleResult:
    """One evaluated sample; a row of the per-sample JSONL.

    `wer` is `edits / reference_words` of `transcript` against the record's text, and
    `None` when the reference normalises to no words. The `batch_*` fields are those
    of the whole batch the sample ran in, numbered `batch_index` in run order.
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
    batch_index: int
    batch_size: int
    batch_encode_s: float
    batch_prefill_s: float
    batch_generate_s: float
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
    return (reasoning.group(1) if reasoning else "", answer_of(generated))


def transcript_of(reasoning: str, task: str) -> str:
    """The reasoning's first paragraph, minus the `continue` instruction the request carries."""
    first = _PARAGRAPH_BREAK.split(reasoning.strip(), maxsplit=1)[0].strip()
    if task == "continue" and first.lower().startswith(CONTINUE_PREFIX.lower()):
        return first[len(CONTINUE_PREFIX) :].strip()
    return first


def _sync(device: torch.device) -> None:
    if device.type == "cuda":
        torch.cuda.synchronize(device)


def _left_pad(rows: Sequence[Sequence[int]]) -> tuple[torch.Tensor, torch.Tensor]:
    """Ids padded on the left with `PAD_TOKEN_ID` to the longest row, and the mask of real ones."""
    width = max(len(row) for row in rows)
    ids = torch.tensor([[PAD_TOKEN_ID] * (width - len(row)) + list(row) for row in rows])
    mask = torch.tensor([[0] * (width - len(row)) + [1] * len(row) for row in rows])
    return ids, mask


@torch.no_grad()
def generate(
    glimmer: FrozenGlimmer,
    prompt_embeds: torch.Tensor,
    attention_mask: torch.Tensor,
    max_new_tokens: int,
    stop_id: int = EOT_ID,
) -> Generation:
    """Greedy decoding from left-padded `prompt_embeds` `(batch, length, hidden)` with a KV cache.

    `attention_mask` `(batch, length)` is 0 on the padding. A row stops after emitting
    `stop_id`, or after `max_new_tokens` ids; see `decode_steps`.
    """
    device = glimmer.device
    rows = prompt_embeds.shape[0]
    ids: list[list[int]] = [[] for _ in range(rows)]
    _sync(device)
    started = time.perf_counter()
    prefill_s = None
    for step in decode_steps(glimmer, prompt_embeds, attention_mask, max_new_tokens, stop_id):
        if prefill_s is None:
            _sync(device)
            prefill_s = time.perf_counter() - started
        for row, new in enumerate(step):
            if new is not None:
                ids[row].append(new)
    finished = [bool(row_ids) and row_ids[-1] == stop_id for row_ids in ids]
    _sync(device)
    return Generation(ids, finished, prefill_s or 0.0, time.perf_counter() - started)


def _read(tokenizer: PromptTokenizer, ids: list[int]) -> tuple[str, str, str]:
    """The decoded response of generated `ids`, its reasoning and its answer."""
    response = GENERATION_PROMPT + tokenizer.decode(ids)
    return (response, *split_reply(response))


def _sample_kl(
    glimmer: FrozenGlimmer, projector: AudioProjector, batch: Batch, samples: int
) -> list[float]:
    """Each sample's mean KL over its own response tokens, from one batched forward."""
    kl = token_losses(glimmer, projector, batch).kl.float()
    rows = batch.student.rows.to(kl.device)
    totals = torch.zeros(samples, device=kl.device).index_add_(0, rows, kl)
    return (totals / torch.bincount(rows, minlength=samples)).tolist()


def evaluate_batch(
    glimmer: FrozenGlimmer,
    tokenizer: PromptTokenizer,
    projector: AudioProjector,
    encoder: WhisperEncoder,
    extractor: WhisperFeatureExtractor,
    records: Sequence[Record],
    clips: Sequence[np.ndarray],
    *,
    histories: Sequence[History],
    index: int,
    max_new_tokens: int,
    with_text_baseline: bool,
) -> list[SampleResult]:
    """Scores `records` together: their KLs, and the students' greedy replies from `clips`.

    The clips are encoded as one batch. `histories[i]` is the earlier text turns in
    front of `records[i]`, for the student and the baseline alike. `index` numbers the
    batch in the results.
    """
    device = glimmer.device
    autocast = torch.autocast(device.type, dtype=torch.bfloat16, enabled=device.type == "cuda")
    started = time.perf_counter()
    frames = [clip.float() for clip in encode(encoder, extractor, clips)]
    encode_s = time.perf_counter() - started
    with torch.no_grad(), autocast:
        batch = make_batch(
            records, frames, tokenizer, projector.audio_token_count, histories=histories
        )
        kls = _sample_kl(glimmer, projector, batch, len(records))
        projected = projector(batch.frames.to(device))
        audio = [projected[i, :count] for i, count in enumerate(batch.audio_tokens)]
        prompts = [
            generation_ids(record.task, AUDIO_TOKEN_TEXT * count, tokenizer, history)
            for record, count, history in zip(records, batch.audio_tokens, histories, strict=True)
        ]
        ids, mask = _left_pad(prompts)
        student = generate(
            glimmer, glimmer.embed_with_audio(ids, audio), mask, max_new_tokens=max_new_tokens
        )
        baselines: list[Baseline | None] = [None] * len(records)
        if with_text_baseline:
            texts = [
                generation_ids(r.task, r.text, tokenizer, h)
                for r, h in zip(records, histories, strict=True)
            ]
            ids, mask = _left_pad(texts)
            teacher = generate(glimmer, glimmer.embed_text(ids), mask, max_new_tokens)
            for i, (new, done) in enumerate(zip(teacher.ids, teacher.finished, strict=True)):
                response, _, answer = _read(tokenizer, new)
                baselines[i] = Baseline(response, answer, done)

    results = []
    for i, record in enumerate(records):
        response, reasoning, answer = _read(tokenizer, student.ids[i])
        transcript = transcript_of(reasoning, record.task)
        reference = normalize(record.text)
        edits = edit_distance(reference, normalize(transcript))
        results.append(
            SampleResult(
                id=record.id,
                task=record.task,
                text=record.text,
                kl=kls[i],
                transcript=transcript,
                edits=edits,
                reference_words=len(reference),
                wer=edits / len(reference) if reference else None,
                answer=answer,
                finished=student.finished[i],
                new_tokens=len(student.ids[i]),
                audio_seconds=frames[i].shape[0] / FRAMES_PER_SECOND,
                batch_index=index,
                batch_size=len(records),
                batch_encode_s=encode_s,
                batch_prefill_s=student.prefill_s,
                batch_generate_s=student.seconds,
                response=response,
                baseline=baselines[i],
            )
        )
    return results


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

    `wer` is total edits over total reference words. `median_batch_prefill_s` is over
    batches, not samples. `generated_tokens_per_s` is the tokens the samples kept
    over the seconds the batches spent generating, prefill included.
    `baseline_finished_rate` appears only when every sample has a text-prompt baseline.
    """
    if not results:
        raise ValueError("no results to summarize")
    by_task: dict[str, list[SampleResult]] = defaultdict(list)
    for result in results:
        by_task[result.task].append(result)
    batches = {r.batch_index: r for r in results}.values()
    summary = _group_summary(results) | {
        "median_batch_prefill_s": statistics.median(b.batch_prefill_s for b in batches),
        "generated_tokens_per_s": sum(r.new_tokens for r in results)
        / sum(b.batch_generate_s for b in batches),
        "by_task": {task: _group_summary(group) for task, group in sorted(by_task.items())},
    }
    baselines = [r.baseline for r in results if r.baseline]
    if len(baselines) == len(results):
        summary["baseline_finished_rate"] = sum(b.finished for b in baselines) / len(baselines)
    return summary


def summarize_latency(results: Sequence[SampleResult]) -> dict[str, Any]:
    """Medians of what one request sees, from `results` that each ran as a batch of one."""
    return {
        "samples": len(results),
        "median_time_to_first_token_s": statistics.median(r.batch_prefill_s for r in results),
        "median_time_to_first_token_with_encoder_s": statistics.median(
            r.batch_encode_s + r.batch_prefill_s for r in results
        ),
    }


def plan_batches(
    records: Sequence[Record],
    clips: Sequence[np.ndarray],
    histories: Sequence[History],
    projector: AudioProjector,
    tokenizer: PromptTokenizer,
    batch_size: int,
) -> list[list[int]]:
    """Record indices in batches of at most `batch_size`, grouped by audio prompt length.

    The prompt counts the record's `histories` entry.
    """
    if batch_size < 1:
        raise ValueError("batch_size must be at least 1")
    lengths = [
        len(
            generation_ids(
                record.task,
                AUDIO_TOKEN_TEXT * projector.audio_token_count(frame_count(len(clip))),
                tokenizer,
                history,
            )
        )
        for record, clip, history in zip(records, clips, histories, strict=True)
    ]
    order = sorted(range(len(records)), key=lengths.__getitem__)
    return [order[start : start + batch_size] for start in range(0, len(order), batch_size)]


def evaluate(
    glimmer: FrozenGlimmer,
    tokenizer: PromptTokenizer,
    projector: AudioProjector,
    encoder: WhisperEncoder,
    extractor: WhisperFeatureExtractor,
    records: Sequence[Record],
    audio: Sequence[Path],
    *,
    histories: Sequence[History],
    batch_size: int,
    max_new_tokens: int,
    with_text_baseline: bool,
    emit: Callable[[list[SampleResult]], None],
    augmenter: Augmenter | None = None,
) -> list[SampleResult]:
    """Evaluates `records` in batches, passing each batch's results to `emit` as it is done.

    `audio[i]` and `histories[i]` are the file and the earlier turns of `records[i]`.
    All clips are read first, and augmented by the `augmenter` if there is one, so
    batches can group similar lengths; the returned results are in the order of `records`.
    """
    clips = [read_audio(path, extractor.n_samples) for path in audio]
    if augmenter is not None:
        clips = [augmenter(clip, index) for index, clip in enumerate(clips)]
    results: dict[int, SampleResult] = {}
    planned = plan_batches(records, clips, histories, projector, tokenizer, batch_size)
    for index, members in enumerate(planned):
        batch = evaluate_batch(
            glimmer,
            tokenizer,
            projector,
            encoder,
            extractor,
            [records[i] for i in members],
            [clips[i] for i in members],
            histories=[histories[i] for i in members],
            index=index,
            max_new_tokens=max_new_tokens,
            with_text_baseline=with_text_baseline,
        )
        results |= dict(zip(members, batch, strict=True))
        emit(batch)
    return [results[i] for i in range(len(records))]


def measure_latency(
    glimmer: FrozenGlimmer,
    tokenizer: PromptTokenizer,
    projector: AudioProjector,
    encoder: WhisperEncoder,
    extractor: WhisperFeatureExtractor,
    records: Sequence[Record],
    audio: Sequence[Path],
    *,
    histories: Sequence[History],
    max_new_tokens: int,
) -> list[SampleResult]:
    """Runs each of `records` alone, as a batch of one: the latency of a single request."""
    return [
        evaluate_batch(
            glimmer,
            tokenizer,
            projector,
            encoder,
            extractor,
            [record],
            [read_audio(path, extractor.n_samples)],
            histories=[history],
            index=index,
            max_new_tokens=max_new_tokens,
            with_text_baseline=False,
        )[0]
        for index, (record, path, history) in enumerate(zip(records, audio, histories, strict=True))
    ]


def load_projector(
    run_dir: Path, text_dim: int, device: torch.device
) -> tuple[AudioProjector, ProjectorConfig]:
    """The trained projector of a run directory, on `device`; its output must be `text_dim` wide."""
    config = read_projector_config(run_dir)
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
    parser.add_argument(
        "--audio-manifest",
        type=Path,
        action="append",
        required=True,
        help="data manifest that holds the records' audio; repeatable",
    )
    parser.add_argument("--whisper-model", default=DEFAULT_MODEL)
    parser.add_argument("--glimmer", type=Path, required=True, help="Glimmer checkpoint")
    parser.add_argument("--projector", type=Path, required=True, help="run dir with the projector")
    parser.add_argument("--out", type=Path, required=True, help="per-sample results JSONL")
    parser.add_argument("--split", default="eval")
    parser.add_argument("--limit", type=int, help="evaluate only the first N records of the split")
    parser.add_argument("--device", choices=["cpu", "cuda"], default="cpu")
    parser.add_argument("--quantize", action="store_true", help="4-bit NF4 decoder; needs cuda")
    parser.add_argument("--max-new-tokens", type=int, default=384)
    parser.add_argument(
        "--batch-size", type=int, default=32, help="samples decoded together, grouped by length"
    )
    parser.add_argument(
        "--latency-samples",
        type=int,
        default=8,
        help="first N samples also run alone, unbatched, for single-request latency",
    )
    parser.add_argument(
        "--history-prob",
        type=float,
        default=0.0,
        help="chance that a sample follows earlier text turns from other evaluated records",
    )
    parser.add_argument(
        "--history-turns",
        type=int,
        default=HistoryConfig.turns,
        help="most earlier turns a sample follows; each history has 1 to this many",
    )
    parser.add_argument(
        "--history-same-task",
        type=float,
        default=HistoryConfig.same_task,
        help="chance that a history is drawn from records of the sample's own task",
    )
    parser.add_argument(
        "--with-text-baseline",
        action="store_true",
        help="also decode from the transcript text with the same weights",
    )
    add_arguments(parser)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = _parse_args(argv)
    if args.quantize and args.device != "cuda":
        raise SystemExit("--quantize needs --device cuda")
    if args.batch_size < 1 or args.latency_samples < 0:
        raise SystemExit("--batch-size must be at least 1 and --latency-samples at least 0")
    records = read_records(args.manifest, args.split, args.limit)
    if not records:
        raise SystemExit(f"no {args.split!r} records in {args.manifest}")
    audio = resolve_audio(records, args.audio_manifest)
    history = HistoryConfig(args.history_prob, args.history_turns, args.history_same_task)
    histories = HistorySampler(records, history, HISTORY_SEED).draw_all()
    augmenter = build_augmenter(args.noise_manifest, args.augment_prob, audio, AUGMENT_SEED)
    glimmer = load_glimmer(args.glimmer, device=args.device, quantize=args.quantize)
    tokenizer = _load_tokenizer(args.glimmer)
    projector, config = load_projector(args.projector, glimmer.hidden_size, glimmer.device)
    encoder, extractor = load_encoder(args.whisper_model, args.device, config.encoder_dim)

    def emit(batch: list[SampleResult]) -> None:
        for result in batch:
            print(json.dumps({"id": result.id, "kl": result.kl, "wer": result.wer}), flush=True)

    results = evaluate(
        glimmer,
        tokenizer,
        projector,
        encoder,
        extractor,
        records,
        audio,
        histories=histories,
        batch_size=args.batch_size,
        max_new_tokens=args.max_new_tokens,
        with_text_baseline=args.with_text_baseline,
        emit=emit,
        augmenter=augmenter,
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows = "".join(json.dumps(asdict(r), ensure_ascii=False) + "\n" for r in results)
    args.out.write_text(rows, encoding="utf-8")
    summary = summarize(results) | {
        "history": asdict(history) | {"seed": HISTORY_SEED},
        "augment": {
            "prob": args.augment_prob,
            "seed": AUGMENT_SEED,
            "noise_manifest": str(args.noise_manifest) if args.noise_manifest else None,
        },
    }
    if args.latency_samples:
        count = args.latency_samples
        alone = measure_latency(
            glimmer,
            tokenizer,
            projector,
            encoder,
            extractor,
            records[:count],
            audio[:count],
            histories=histories[:count],
            max_new_tokens=args.max_new_tokens,
        )
        summary["single_request"] = summarize_latency(alone)
    text = json.dumps(summary, indent=2)
    args.out.with_suffix(SUMMARY_SUFFIX).write_text(text + "\n", encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
