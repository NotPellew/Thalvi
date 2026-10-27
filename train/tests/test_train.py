import json
import random
from pathlib import Path

import pytest
import torch
import torch.nn.functional as F
from safetensors.torch import load_file
from tiny_glimmer import HIDDEN, FakeTokenizer
from tiny_whisper import DIM, write_audio_manifest
from transformers import MuseGlimmerForConditionalGeneration

from thalvi_train import train
from thalvi_train.batch import make_batch
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.projector import AudioProjector
from thalvi_train.records import Record, resolve_audio
from thalvi_train.train import (
    PROJECTOR_CONFIG,
    PROJECTOR_WEIGHTS,
    ProjectorConfig,
    TrainConfig,
    _step_groups,
    distill_loss,
    lr_at,
    run,
)

PROJECTOR = ProjectorConfig(encoder_dim=DIM, hidden_dim=32, text_dim=HIDDEN, stack=4)
RESPONSE = (
    "<|start|>assistant to=self<|message|>hmm<|eom|><|start|>assistant to=user<|message|>yes<|eot|>"
)


@pytest.fixture
def glimmer(tiny_glimmer_dir: Path) -> FrozenGlimmer:
    return load_glimmer(tiny_glimmer_dir, dtype=torch.float32)


CLIP_SAMPLES = {"a": 3200, "b": 1600}


@pytest.fixture
def audio_manifest(tmp_path: Path) -> Path:
    return write_audio_manifest(tmp_path / "speech", CLIP_SAMPLES)


RECORDS = [
    Record("a", "what is two plus two", "answer", "train", RESPONSE),
    Record("b", "once upon a time", "continue", "train", RESPONSE),
]


def train_config(out_dir: Path, **overrides) -> TrainConfig:
    settings = {"warmup_steps": 1, "lr": 3e-2, "save_every": 1000, "kl_chunk": 4} | overrides
    return TrainConfig(out_dir=out_dir, **settings)


def test_lr_warms_up_then_cosines_to_zero() -> None:
    lrs = [lr_at(step, total_steps=10, warmup_steps=2, peak=1.0) for step in range(10)]
    assert lrs[:2] == [0.5, 1.0]
    assert lrs[2] == pytest.approx(1.0)
    assert lrs[1:] == sorted(lrs[1:], reverse=True)
    assert lrs[-1] == pytest.approx(0.0, abs=0.05)


def reference_losses(
    reference: MuseGlimmerForConditionalGeneration, glimmer: FrozenGlimmer, projector, batch
):
    """KL and CE from the full HF model's own logits, one sample at a time."""
    kls, ces = [], []
    audio = projector(batch.frames)
    for i, count in enumerate(batch.audio_tokens):
        t_ids = batch.teacher.input_ids[i : i + 1]
        s_embeds = glimmer.embed_with_audio(batch.student.input_ids[i : i + 1], [audio[i, :count]])
        t_len = int(batch.teacher.attention_mask[i].sum())
        s_len = int(batch.student.attention_mask[i].sum())
        teacher = reference(input_ids=t_ids[:, :t_len]).logits[0]
        student = reference(inputs_embeds=s_embeds[:, :s_len]).logits[0]
        t_cols = batch.teacher.cols[batch.teacher.rows == i]
        s_cols = batch.student.cols[batch.student.rows == i]
        targets = batch.targets[batch.teacher.rows == i]
        t_logp = F.log_softmax(teacher[t_cols], dim=-1)
        s_logp = F.log_softmax(student[s_cols], dim=-1)
        kls.append((t_logp.exp() * (t_logp - s_logp)).sum(-1))
        ces.append(-s_logp[torch.arange(len(targets)), targets])
    return torch.cat(kls).mean(), torch.cat(ces).mean()


def test_loss_matches_kl_and_ce_computed_from_the_full_model(
    glimmer: FrozenGlimmer,
    reference_model: MuseGlimmerForConditionalGeneration,
) -> None:
    torch.manual_seed(2)
    projector = AudioProjector(**PROJECTOR.__dict__)
    frames = [torch.randn(10, DIM), torch.randn(5, DIM)]
    batch = make_batch(RECORDS, frames, FakeTokenizer(), projector.audio_token_count)
    parts = distill_loss(glimmer, projector, batch, kl_chunk=3)
    kl, ce = reference_losses(reference_model, glimmer, projector, batch)
    assert parts.kl.item() > 0
    assert parts.kl.item() == pytest.approx(kl.item(), rel=1e-4)
    assert parts.ce.item() == pytest.approx(ce.item(), rel=1e-4)


def run_on(glimmer, encoder, audio_manifest, records, config, emit=lambda _: None):
    audio = resolve_audio(records, [audio_manifest])
    return run(glimmer, FakeTokenizer(), records, audio, *encoder, PROJECTOR, config, emit)


def test_training_lowers_the_loss_and_changes_only_the_projector(
    glimmer: FrozenGlimmer, tiny_encoder, audio_manifest: Path, tmp_path: Path
) -> None:
    before = {k: v.clone() for k, v in glimmer.state_dict().items()}
    encoder_before = {k: v.clone() for k, v in tiny_encoder[0].state_dict().items()}
    lines: list[dict] = []
    config = train_config(tmp_path / "out", grad_accum=1, epochs=40, max_steps=40)
    projector = run_on(glimmer, tiny_encoder, audio_manifest, RECORDS[:1], config, lines.append)

    assert [line["step"] for line in lines] == list(range(1, 41))
    assert set(lines[0]) == {"step", "loss", "kl", "lr", "tokens_per_s"}
    assert lines[-1]["loss"] < 0.5 * lines[0]["loss"]
    assert all(torch.equal(v, glimmer.state_dict()[k]) for k, v in before.items())
    assert all(torch.equal(v, tiny_encoder[0].state_dict()[k]) for k, v in encoder_before.items())
    untrained = AudioProjector(**PROJECTOR.__dict__)
    assert any(
        not torch.equal(p, q)
        for p, q in zip(projector.parameters(), untrained.parameters(), strict=True)
    )


def test_saves_intermediate_and_final_projector_with_its_config(
    glimmer: FrozenGlimmer, tiny_encoder, audio_manifest: Path, tmp_path: Path
) -> None:
    out = tmp_path / "out"
    config = train_config(out, grad_accum=1, epochs=3, max_steps=5, save_every=2)
    projector = run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config)

    assert {p.name for p in out.iterdir()} == {
        PROJECTOR_CONFIG,
        "projector-step2.safetensors",
        "projector-step4.safetensors",
        PROJECTOR_WEIGHTS,
    }
    assert json.loads((out / PROJECTOR_CONFIG).read_text()) == PROJECTOR.__dict__
    saved = load_file(out / PROJECTOR_WEIGHTS)
    assert all(torch.equal(saved[k], v) for k, v in projector.state_dict().items())


def test_accumulates_micro_batches_into_each_step(
    glimmer: FrozenGlimmer, tiny_encoder, audio_manifest: Path, tmp_path: Path
) -> None:
    lines: list[dict] = []
    config = train_config(tmp_path / "out", batch_size=1, grad_accum=2, epochs=2)
    run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config, lines.append)
    assert [line["step"] for line in lines] == [1, 2]


def test_every_micro_batch_is_encoded_from_its_own_records_audio(
    glimmer: FrozenGlimmer,
    tiny_encoder,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    encoded: list[int] = []
    batched: list[str] = []
    real_encode, real_make_batch = train.encode, train.make_batch

    def spy_encode(encoder, extractor, clips):
        encoded.extend(len(clip) for clip in clips)
        return real_encode(encoder, extractor, clips)

    def spy_make_batch(records, frames, *args):
        batched.extend(r.id for r in records)
        return real_make_batch(records, frames, *args)

    monkeypatch.setattr(train, "encode", spy_encode)
    monkeypatch.setattr(train, "make_batch", spy_make_batch)
    config = train_config(tmp_path / "out", grad_accum=1, epochs=3)
    run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config)

    assert len(batched) == 6
    assert encoded == [CLIP_SAMPLES[id] for id in batched]


def test_gradient_checkpointing_leaves_projector_gradients_unchanged(
    glimmer: FrozenGlimmer,
) -> None:
    torch.manual_seed(3)
    projector = AudioProjector(**PROJECTOR.__dict__)
    batch = make_batch(
        RECORDS,
        [torch.randn(10, DIM), torch.randn(5, DIM)],
        FakeTokenizer(),
        projector.audio_token_count,
    )

    def gradients() -> list[torch.Tensor]:
        projector.zero_grad()
        parts = distill_loss(glimmer, projector, batch, kl_chunk=3)
        (parts.kl + parts.ce).backward()
        return [p.grad.clone() for p in projector.parameters()]

    plain = gradients()
    glimmer.enable_gradient_checkpointing()
    checkpointed = gradients()
    assert all(
        torch.allclose(a, b, atol=1e-6, rtol=1e-4) for a, b in zip(plain, checkpointed, strict=True)
    )


def test_step_groups_cover_every_record_once_in_length_sorted_batches() -> None:
    lengths = [random.Random(i).randrange(1000) for i in range(500)]
    groups = _step_groups(lengths, batch_size=4, grad_accum=2, rng=random.Random(0))
    batches = [batch for group in groups for batch in group]
    assert sorted(i for batch in batches for i in batch) == list(range(500))
    assert all(len(group) <= 2 for group in groups)
    spread = sum(max(lengths[i] for i in b) - min(lengths[i] for i in b) for b in batches)
    assert spread / len(batches) < 100
