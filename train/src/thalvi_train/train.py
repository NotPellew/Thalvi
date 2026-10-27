"""Distillation training CLI: Glimmer reading the transcript teaches Glimmer hearing audio.

Only the projector trains. Per response token the loss is KL(teacher || student)
at temperature 1, with an optional cross-entropy against the response tokens.
Logits exist only at response positions, a chunk at a time. The frozen Whisper
encoder runs inline on the training device, one micro-batch ahead of the projector.
"""

import argparse
import json
import math
import random
import time
from collections.abc import Callable, Iterator, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import asdict, dataclass
from pathlib import Path

import numpy as np
import torch
import torch.nn.functional as F
from safetensors.torch import save_file
from torch.utils.checkpoint import checkpoint
from transformers import AutoTokenizer, WhisperFeatureExtractor
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.batch import Batch, make_batch
from thalvi_train.features import DEFAULT_MODEL, encode, load_encoder, read_audio
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
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


@dataclass(frozen=True)
class TrainConfig:
    """Optimisation settings. A step is one optimiser update over `grad_accum` micro-batches."""

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
    seed: int = 0


@dataclass(frozen=True)
class LossParts:
    """Token-mean KL and cross-entropy of one micro-batch."""

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
    kl = F.kl_div(student, teacher, reduction="sum", log_target=True)
    ce = F.nll_loss(student, targets, reduction="sum")
    return kl, ce


def distill_loss(
    glimmer: FrozenGlimmer, projector: AudioProjector, batch: Batch, kl_chunk: int = 64
) -> LossParts:
    """Teacher-vs-student loss for `batch`, with gradients into `projector` only.

    Each chunk of response positions is recomputed in backward, so full-vocab
    logits for at most `kl_chunk` tokens are alive at once.
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
    kl = ce = torch.zeros((), device=device)
    for start in range(0, len(targets), kl_chunk):
        window = slice(start, start + kl_chunk)
        chunk_kl, chunk_ce = checkpoint(
            _chunk_losses,
            glimmer,
            teacher_hidden[window],
            student_hidden[window],
            targets[window],
            use_reentrant=False,
        )
        kl, ce = kl + chunk_kl, ce + chunk_ce
    return LossParts(kl=kl / len(targets), ce=ce / len(targets))


def _save_projector(projector: AudioProjector, path: Path) -> None:
    save_file({k: v.detach().cpu().contiguous() for k, v in projector.state_dict().items()}, path)


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
    """Trains a fresh projector on `records`, emitting one dict per step and saving checkpoints.

    `audio[i]` is the file of `records[i]`. Each micro-batch is read, run through the
    frozen `encoder` without gradients, and only then through the projector.
    """
    if not records:
        raise ValueError("no records to train on")
    torch.manual_seed(config.seed)
    rng = random.Random(config.seed)
    device = glimmer.device
    projector = AudioProjector(**asdict(projector_config)).to(device)
    optimizer = torch.optim.AdamW(
        projector.parameters(), lr=config.lr, weight_decay=config.weight_decay
    )
    glimmer.enable_gradient_checkpointing()
    config.out_dir.mkdir(parents=True, exist_ok=True)
    (config.out_dir / PROJECTOR_CONFIG).write_text(json.dumps(asdict(projector_config)))

    lengths = [len(record.text) + len(record.response) for record in records]
    epochs = [
        _step_groups(lengths, config.batch_size, config.grad_accum, rng)
        for _ in range(config.epochs)
    ]
    plan = [group for groups in epochs for group in groups]
    total_steps = min(len(plan), config.max_steps or len(plan))
    plan = plan[:total_steps]
    autocast = torch.autocast(device.type, dtype=torch.bfloat16, enabled=device.type == "cuda")

    with ThreadPoolExecutor(READ_THREADS) as pool:
        clips = _read_ahead(
            pool,
            [[audio[i] for i in indices] for group in plan for indices in group],
            extractor.n_samples,
        )
        for step, group in enumerate(plan, start=1):
            lr = lr_at(step - 1, total_steps, config.warmup_steps, config.lr)
            for param_group in optimizer.param_groups:
                param_group["lr"] = lr
            started = time.perf_counter()
            loss_sum = kl_sum = tokens = 0.0
            for indices in group:
                chosen = [records[i] for i in indices]
                frames = [f.float() for f in encode(encoder, extractor, next(clips))]
                batch = make_batch(chosen, frames, tokenizer, projector.audio_token_count)
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
    parser.add_argument("--encoder-dim", type=int, default=ProjectorConfig.encoder_dim)
    parser.add_argument("--hidden-dim", type=int, default=ProjectorConfig.hidden_dim)
    parser.add_argument("--stack", type=int, default=ProjectorConfig.stack)
    parser.add_argument("--seed", type=int, default=0)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = _parse_args(argv)
    if args.quantize and args.device != "cuda":
        raise SystemExit("--quantize needs --device cuda")
    records = read_records(args.manifest, args.split, args.limit)
    audio = resolve_audio(records, args.audio_manifest)
    encoder, extractor = load_encoder(args.whisper_model, args.device, args.encoder_dim)
    glimmer = load_glimmer(args.glimmer, device=args.device, quantize=args.quantize)
    tokenizer = GlimmerTokenizer(AutoTokenizer.from_pretrained(args.glimmer))
    projector_config = ProjectorConfig(
        encoder_dim=args.encoder_dim,
        hidden_dim=args.hidden_dim,
        text_dim=glimmer.hidden_size,
        stack=args.stack,
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
        seed=args.seed,
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
