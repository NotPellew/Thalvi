"""Distillation training CLI: Glimmer reading the transcript teaches Glimmer hearing audio.

Only the projector trains. Per response token the loss is KL(teacher || student)
at temperature 1, with an optional cross-entropy against the response tokens.
Logits exist only at response positions, a chunk at a time. The frozen Whisper
encoder runs inline on the training device, one micro-batch ahead of the projector.

Some samples get earlier text turns drawn from other records (see `history`), the same
for teacher and student, so the projector learns to hear the audio with text in context.
`--init-projector` continues from an earlier run's weights.
"""

import argparse
import json
import math
import random
import time
from collections.abc import Callable, Iterator, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import asdict, dataclass, field
from pathlib import Path

import numpy as np
import torch
import torch.nn.functional as F
from safetensors.torch import load_file, save_file
from torch.utils.checkpoint import checkpoint
from transformers import AutoTokenizer, WhisperFeatureExtractor
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.batch import Batch, make_batch
from thalvi_train.features import DEFAULT_MODEL, encode, load_encoder, read_audio
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.history import HistoryConfig, HistorySampler, history_chars
from thalvi_train.projector import AudioProjector
from thalvi_train.prompts import GlimmerTokenizer, PromptTokenizer
from thalvi_train.records import Record, read_records, resolve_audio

PROJECTOR_WEIGHTS = "projector.safetensors"
PROJECTOR_CONFIG = "projector.json"
BUCKET_WINDOW = 64
READ_THREADS = 4


@dataclass(frozen=True)
class ProjectorConfig:
    """Constructor arguments of `AudioProjector`; saved next to its weights."""

    encoder_dim: int = 1280
    hidden_dim: int = 4096
    text_dim: int = 6656
    stack: int = 8


def read_projector_config(run_dir: Path) -> ProjectorConfig:
    """The `ProjectorConfig` saved in a run directory."""
    return ProjectorConfig(**json.loads((run_dir / PROJECTOR_CONFIG).read_text()))


@dataclass(frozen=True)
class TrainConfig:
    """Optimisation settings. A step is one optimiser update over `grad_accum` micro-batches.

    `init_projector` is a run directory whose weights the projector starts from; its
    saved config must equal the one being trained. Warmup applies as for a fresh start.
    """

    out_dir: Path
    batch_size: int = 1
    grad_accum: int = 16
    epochs: int = 1
    max_steps: int | None = None
    lr: float = 1e-4
    weight_decay: float = 0.01
    warmup_steps: int = 100
    max_grad_norm: float = 1.0
    ce_weight: float = 0.0
    save_every: int = 500
    kl_chunk: int = 64
    max_response_tokens: int | None = None
    seed: int = 0
    history: HistoryConfig = field(default_factory=HistoryConfig)
    init_projector: Path | None = None


@dataclass(frozen=True)
class LossParts:
    """KL and cross-entropy of a micro-batch: token means, or one value per response token."""

    kl: torch.Tensor
    ce: torch.Tensor


def lr_at(step: int, total_steps: int, warmup_steps: int, peak: float) -> float:
    """Linear warmup to `peak` over `warmup_steps`, then cosine decay to zero at `total_steps`."""
    if step < warmup_steps:
        return peak * (step + 1) / warmup_steps
    progress = (step - warmup_steps) / max(1, total_steps - warmup_steps)
    return peak * 0.5 * (1 + math.cos(math.pi * min(progress, 1.0)))


def _chunk_losses(
    glimmer: FrozenGlimmer,
    teacher_hidden: torch.Tensor,
    student_hidden: torch.Tensor,
    targets: torch.Tensor,
) -> tuple[torch.Tensor, torch.Tensor]:
    with torch.no_grad():
        teacher = F.log_softmax(glimmer.logits(teacher_hidden), dim=-1)
    student = F.log_softmax(glimmer.logits(student_hidden), dim=-1)
    kl = F.kl_div(student, teacher, reduction="none", log_target=True).sum(dim=-1)
    ce = F.nll_loss(student, targets, reduction="none")
    return kl, ce


def token_losses(
    glimmer: FrozenGlimmer, projector: AudioProjector, batch: Batch, kl_chunk: int = 64
) -> LossParts:
    """Teacher-vs-student KL and cross-entropy of each response token of `batch`.

    Both are `(tokens,)`, in `batch.student.rows` order, with gradients into
    `projector` only. Each chunk of response positions is recomputed in
    backward, so full-vocab logits for at most `kl_chunk` tokens are alive at once.
    """
    device = glimmer.device
    teacher, student = batch.teacher, batch.student
    with torch.no_grad():
        states = glimmer.hidden_states(
            glimmer.embed_text(teacher.input_ids), teacher.attention_mask.to(device)
        )
        teacher_hidden = states[teacher.rows.to(device), teacher.cols.to(device)]

    projected = projector(batch.frames.to(device))
    audio = [projected[i, :count] for i, count in enumerate(batch.audio_tokens)]
    states = glimmer.hidden_states(
        glimmer.embed_with_audio(student.input_ids, audio), student.attention_mask.to(device)
    )
    student_hidden = states[student.rows.to(device), student.cols.to(device)]

    targets = batch.targets.to(device)
    chunks = [
        checkpoint(
            _chunk_losses,
            glimmer,
            teacher_hidden[window],
            student_hidden[window],
            targets[window],
            use_reentrant=False,
        )
        for window in (slice(i, i + kl_chunk) for i in range(0, len(targets), kl_chunk))
    ]
    return LossParts(kl=torch.cat([kl for kl, _ in chunks]), ce=torch.cat([ce for _, ce in chunks]))


def distill_loss(
    glimmer: FrozenGlimmer, projector: AudioProjector, batch: Batch, kl_chunk: int = 64
) -> LossParts:
    """Token-mean teacher-vs-student loss for `batch`, with gradients into `projector` only."""
    tokens = token_losses(glimmer, projector, batch, kl_chunk)
    return LossParts(kl=tokens.kl.mean(), ce=tokens.ce.mean())


def _save_projector(projector: AudioProjector, path: Path) -> None:
    save_file({k: v.detach().cpu().contiguous() for k, v in projector.state_dict().items()}, path)


def _load_init(run_dir: Path, expected: ProjectorConfig) -> dict[str, torch.Tensor]:
    saved = read_projector_config(run_dir)
    if saved != expected:
        raise ValueError(f"{run_dir}: projector config {saved} does not match {expected}")
    return load_file(run_dir / PROJECTOR_WEIGHTS)


def _step_groups(
    lengths: Sequence[int], batch_size: int, grad_accum: int, rng: random.Random
) -> list[list[list[int]]]:
    """One epoch of record indices, as optimiser steps of micro-batches.

    Records are shuffled, sorted by `lengths` within windows of `BUCKET_WINDOW`
    micro-batches so each micro-batch pads little, and the micro-batches are
    shuffled again so lengths don't drift through the epoch.
    """
    order = list(range(len(lengths)))
    rng.shuffle(order)
    window = BUCKET_WINDOW * batch_size
    batches = []
    for start in range(0, len(order), window):
        bucket = sorted(order[start : start + window], key=lambda i: lengths[i])
        batches += [bucket[i : i + batch_size] for i in range(0, len(bucket), batch_size)]
    rng.shuffle(batches)
    return [batches[i : i + grad_accum] for i in range(0, len(batches), grad_accum)]


def _read_ahead(
    pool: ThreadPoolExecutor, batches: Sequence[Sequence[Path]], max_samples: int
) -> Iterator[list[np.ndarray]]:
    """Yields each batch's clips, already reading the next batch while the caller works."""

    def submit(paths: Sequence[Path]) -> list[Future[np.ndarray]]:
        return [pool.submit(read_audio, path, max_samples) for path in paths]

    pending = submit(batches[0]) if batches else []
    for index in range(len(batches)):
        current = pending
        if index + 1 < len(batches):
            pending = submit(batches[index + 1])
        yield [future.result() for future in current]


def run(
    glimmer: FrozenGlimmer,
    tokenizer: PromptTokenizer,
    records: list[Record],
    audio: Sequence[Path],
    encoder: WhisperEncoder,
    extractor: WhisperFeatureExtractor,
    projector_config: ProjectorConfig,
    config: TrainConfig,
    emit: Callable[[dict[str, float | int]], None],
) -> AudioProjector:
    """Trains a projector on `records`, emitting one dict per step and saving checkpoints.

    The projector is fresh, or loaded from `config.init_projector`. `audio[i]` is the
    file of `records[i]`. Each micro-batch is read, run through the frozen `encoder`
    without gradients, and only then through the projector. Every epoch draws its own
    histories, and the length bucketing counts them.
    """
    if not records:
        raise ValueError("no records to train on")
    torch.manual_seed(config.seed)
    rng = random.Random(config.seed)
    device = glimmer.device
    projector = AudioProjector(**asdict(projector_config))
    if config.init_projector is not None:
        projector.load_state_dict(_load_init(config.init_projector, projector_config))
    projector = projector.to(device)
    optimizer = torch.optim.AdamW(
        projector.parameters(), lr=config.lr, weight_decay=config.weight_decay
    )
    glimmer.enable_gradient_checkpointing()
    config.out_dir.mkdir(parents=True, exist_ok=True)
    (config.out_dir / PROJECTOR_CONFIG).write_text(json.dumps(asdict(projector_config)))

    sampler = HistorySampler(records, config.history, config.seed)
    histories = [sampler.draw_all(epoch) for epoch in range(config.epochs)]
    epochs = []
    for epoch_histories in histories:
        lengths = [
            len(record.text) + len(record.response) + history_chars(history)
            for record, history in zip(records, epoch_histories, strict=True)
        ]
        epochs.append(_step_groups(lengths, config.batch_size, config.grad_accum, rng))
    plan = [(epoch, group) for epoch, groups in enumerate(epochs) for group in groups]
    total_steps = min(len(plan), config.max_steps or len(plan))
    plan = plan[:total_steps]
    autocast = torch.autocast(device.type, dtype=torch.bfloat16, enabled=device.type == "cuda")

    with ThreadPoolExecutor(READ_THREADS) as pool:
        clips = _read_ahead(
            pool,
            [[audio[i] for i in indices] for _, group in plan for indices in group],
            extractor.n_samples,
        )
        for step, (epoch, group) in enumerate(plan, start=1):
            lr = lr_at(step - 1, total_steps, config.warmup_steps, config.lr)
            for param_group in optimizer.param_groups:
                param_group["lr"] = lr
            started = time.perf_counter()
            loss_sum = kl_sum = tokens = 0.0
            for indices in group:
                chosen = [records[i] for i in indices]
                frames = [f.float() for f in encode(encoder, extractor, next(clips))]
                batch = make_batch(
                    chosen,
                    frames,
                    tokenizer,
                    projector.audio_token_count,
                    config.max_response_tokens,
                    histories=[histories[epoch][i] for i in indices],
                )
                with autocast:
                    parts = distill_loss(glimmer, projector, batch, config.kl_chunk)
                    loss = parts.kl + config.ce_weight * parts.ce
                (loss / len(group)).backward()
                loss_sum += loss.item() / len(group)
                kl_sum += parts.kl.item() / len(group)
                tokens += batch.student.tokens
            torch.nn.utils.clip_grad_norm_(projector.parameters(), config.max_grad_norm)
            optimizer.step()
            optimizer.zero_grad(set_to_none=True)
            emit(
                {
                    "step": step,
                    "loss": loss_sum,
                    "kl": kl_sum,
                    "lr": lr,
                    "tokens_per_s": tokens / (time.perf_counter() - started),
                }
            )
            if step % config.save_every == 0 and step != total_steps:
                _save_projector(projector, config.out_dir / f"projector-step{step}.safetensors")
    _save_projector(projector, config.out_dir / PROJECTOR_WEIGHTS)
    return projector


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument(
        "--audio-manifest",
        type=Path,
        action="append",
        required=True,
        help="data manifest that holds the records' audio; repeatable",
    )
    parser.add_argument("--whisper-model", default=DEFAULT_MODEL)
    parser.add_argument("--glimmer", type=Path, required=True, help="full Glimmer checkpoint")
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--device", choices=["cpu", "cuda"], default="cpu")
    parser.add_argument("--quantize", action="store_true", help="4-bit NF4 decoder; needs cuda")
    parser.add_argument("--split", default="train")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--max-steps", type=int)
    parser.add_argument("--epochs", type=int, default=1)
    parser.add_argument("--batch-size", type=int, default=1)
    parser.add_argument("--grad-accum", type=int, default=16)
    parser.add_argument("--lr", type=float, default=1e-4)
    parser.add_argument("--weight-decay", type=float, default=0.01)
    parser.add_argument("--warmup-steps", type=int, default=100)
    parser.add_argument("--max-grad-norm", type=float, default=1.0)
    parser.add_argument("--ce-weight", type=float, default=0.0)
    parser.add_argument("--save-every", type=int, default=500)
    parser.add_argument("--kl-chunk", type=int, default=64)
    parser.add_argument("--max-response-tokens", type=int, help="train on the first N only")
    parser.add_argument(
        "--encoder-dim", type=int, help=f"default {ProjectorConfig.encoder_dim}, or the init's"
    )
    parser.add_argument(
        "--hidden-dim", type=int, help=f"default {ProjectorConfig.hidden_dim}, or the init's"
    )
    parser.add_argument("--stack", type=int, help=f"default {ProjectorConfig.stack}, or the init's")
    parser.add_argument(
        "--init-projector",
        type=Path,
        help="run dir whose projector weights and config training starts from",
    )
    parser.add_argument(
        "--history-prob",
        type=float,
        default=HistoryConfig.prob,
        help="chance that a sample gets earlier text turns from other records",
    )
    parser.add_argument(
        "--history-turns",
        type=int,
        default=HistoryConfig.turns,
        help="most earlier turns a sample gets; each history has 1 to this many",
    )
    parser.add_argument(
        "--history-same-task",
        type=float,
        default=HistoryConfig.same_task,
        help="chance that a history is drawn from records of the sample's own task",
    )
    parser.add_argument("--seed", type=int, default=0)
    return parser.parse_args(argv)


def _load_tokenizer(path: Path) -> PromptTokenizer:
    return GlimmerTokenizer(AutoTokenizer.from_pretrained(path))


def main(argv: list[str] | None = None) -> None:
    args = _parse_args(argv)
    if args.quantize and args.device != "cuda":
        raise SystemExit("--quantize needs --device cuda")
    records = read_records(args.manifest, args.split, args.limit)
    audio = resolve_audio(records, args.audio_manifest)
    base = ProjectorConfig()
    if args.init_projector is not None:
        base = read_projector_config(args.init_projector)
    encoder_dim = base.encoder_dim if args.encoder_dim is None else args.encoder_dim
    encoder, extractor = load_encoder(args.whisper_model, args.device, encoder_dim)
    glimmer = load_glimmer(args.glimmer, device=args.device, quantize=args.quantize)
    tokenizer = _load_tokenizer(args.glimmer)
    projector_config = ProjectorConfig(
        encoder_dim=encoder_dim,
        hidden_dim=base.hidden_dim if args.hidden_dim is None else args.hidden_dim,
        text_dim=glimmer.hidden_size,
        stack=base.stack if args.stack is None else args.stack,
    )
    config = TrainConfig(
        out_dir=args.out_dir,
        batch_size=args.batch_size,
        grad_accum=args.grad_accum,
        epochs=args.epochs,
        max_steps=args.max_steps,
        lr=args.lr,
        weight_decay=args.weight_decay,
        warmup_steps=args.warmup_steps,
        max_grad_norm=args.max_grad_norm,
        ce_weight=args.ce_weight,
        save_every=args.save_every,
        kl_chunk=args.kl_chunk,
        max_response_tokens=args.max_response_tokens,
        seed=args.seed,
        history=HistoryConfig(args.history_prob, args.history_turns, args.history_same_task),
        init_projector=args.init_projector,
    )
    run(
        glimmer,
        tokenizer,
        records,
        audio,
        encoder,
        extractor,
        projector_config,
        config,
        lambda line: print(json.dumps(line), flush=True),
    )


if __name__ == "__main__":
    main()
