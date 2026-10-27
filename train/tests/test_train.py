import json
import random
from pathlib import Path

import numpy as np
import pytest
import soundfile
import torch
import torch.nn.functional as F
from safetensors.torch import load_file, save_file
from tiny_glimmer import HIDDEN, FakeTokenizer
from tiny_noise import write_noise_manifest
from tiny_whisper import DIM, write_audio_manifest
from transformers import MuseGlimmerForConditionalGeneration

from thalvi_train import train
from thalvi_train.augment import Augmenter, read_noise_manifest
from thalvi_train.batch import make_batch
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.history import HistoryConfig, history_chars
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


def run_on(glimmer, encoder, audio_manifest, records, config, emit=lambda _: None, augment=None):
    audio = resolve_audio(records, [audio_manifest])
    augmenter = augment(audio) if augment else None
    return run(
        glimmer, FakeTokenizer(), records, audio, *encoder, PROJECTOR, config, emit, augmenter
    )


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

    def spy_make_batch(records, frames, *args, **kwargs):
        batched.extend(r.id for r in records)
        return real_make_batch(records, frames, *args, **kwargs)

    monkeypatch.setattr(train, "encode", spy_encode)
    monkeypatch.setattr(train, "make_batch", spy_make_batch)
    config = train_config(tmp_path / "out", grad_accum=1, epochs=3)
    run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config)

    assert len(batched) == 6
    assert encoded == [CLIP_SAMPLES[id] for id in batched]


def test_augmented_clips_reach_the_encoder_and_only_the_projector_trains(
    glimmer: FrozenGlimmer,
    tiny_encoder,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    before = {k: v.clone() for k, v in glimmer.state_dict().items()}
    encoder_before = {k: v.clone() for k, v in tiny_encoder[0].state_dict().items()}
    noise = write_noise_manifest(tmp_path / "noise", {"hiss": ("noise", 4000)})
    encoded: list[np.ndarray] = []
    real_encode = train.encode

    def spy_encode(encoder, extractor, clips):
        encoded.extend(clips)
        return real_encode(encoder, extractor, clips)

    monkeypatch.setattr(train, "encode", spy_encode)
    lines: list[dict] = []
    config = train_config(tmp_path / "out", grad_accum=1, epochs=3)
    projector = run_on(
        glimmer,
        tiny_encoder,
        audio_manifest,
        RECORDS,
        config,
        lines.append,
        augment=lambda audio: Augmenter(
            read_noise_manifest(noise), audio, prob=1.0, seed=config.seed
        ),
    )

    plain = {n: soundfile.read(tmp_path / "speech" / "audio" / f"{n}.wav")[0] for n in "ab"}
    assert len(encoded) == 6 and len(lines) == 6
    assert sorted(len(clip) for clip in encoded) == [1600] * 3 + [3200] * 3
    assert all(
        not np.allclose(clip, plain["a" if len(clip) == 3200 else "b"], atol=1e-4)
        for clip in encoded
    )
    assert all(torch.equal(v, glimmer.state_dict()[k]) for k, v in before.items())
    assert all(torch.equal(v, tiny_encoder[0].state_dict()[k]) for k, v in encoder_before.items())
    untrained = AudioProjector(**PROJECTOR.__dict__)
    assert any(
        not torch.equal(p, q)
        for p, q in zip(projector.parameters(), untrained.parameters(), strict=True)
    )


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


def test_history_reaches_both_prompts_and_training_still_lowers_the_loss(
    glimmer: FrozenGlimmer,
    tiny_encoder,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    before = {k: v.clone() for k, v in glimmer.state_dict().items()}
    encoder_before = {k: v.clone() for k, v in tiny_encoder[0].state_dict().items()}
    seen: list[list[tuple]] = []
    real_make_batch = train.make_batch

    def spy_make_batch(*args, histories, **kwargs):
        seen.extend(histories)
        return real_make_batch(*args, histories=histories, **kwargs)

    monkeypatch.setattr(train, "make_batch", spy_make_batch)
    lines: list[dict] = []
    config = train_config(
        tmp_path / "out",
        batch_size=2,
        grad_accum=1,
        epochs=40,
        max_steps=40,
        history=HistoryConfig(prob=1.0, turns=1),
    )
    projector = run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config, lines.append)

    assert len(lines) == 40
    assert lines[-1]["loss"] < 0.5 * lines[0]["loss"]
    assert sorted({history[0][0] for history in seen}) == [
        "once upon a time",
        "what is two plus two",
    ]
    assert all(len(history) == 1 for history in seen)
    assert all(torch.equal(v, glimmer.state_dict()[k]) for k, v in before.items())
    assert all(torch.equal(v, tiny_encoder[0].state_dict()[k]) for k, v in encoder_before.items())
    untrained = AudioProjector(**PROJECTOR.__dict__)
    assert any(
        not torch.equal(p, q)
        for p, q in zip(projector.parameters(), untrained.parameters(), strict=True)
    )


def test_length_bucketing_counts_the_history(
    glimmer: FrozenGlimmer,
    tiny_encoder,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen: list[list[int]] = []
    real_groups = train._step_groups

    def spy_groups(lengths, *args):
        seen.append(list(lengths))
        return real_groups(lengths, *args)

    monkeypatch.setattr(train, "_step_groups", spy_groups)
    for prob in (0.0, 1.0):
        config = train_config(
            tmp_path / "out", grad_accum=1, history=HistoryConfig(prob=prob, turns=1)
        )
        run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config)

    plain, with_history = seen
    assert plain == [len(r.text) + len(r.response) for r in RECORDS]
    extra = [history_chars(((RECORDS[1 - i].text, "yes"),)) for i in range(2)]
    assert [b - a for a, b in zip(plain, with_history, strict=True)] == extra


def save_run(directory: Path, config: ProjectorConfig, seed: int = 5) -> dict[str, torch.Tensor]:
    torch.manual_seed(seed)
    weights = AudioProjector(**config.__dict__).state_dict()
    directory.mkdir()
    (directory / PROJECTOR_CONFIG).write_text(json.dumps(config.__dict__))
    save_file(weights, directory / PROJECTOR_WEIGHTS)
    return weights


def test_init_projector_starts_from_the_saved_weights(
    glimmer: FrozenGlimmer, tiny_encoder, audio_manifest: Path, tmp_path: Path
) -> None:
    weights = save_run(tmp_path / "init", PROJECTOR)
    still = train_config(tmp_path / "still", lr=0.0, init_projector=tmp_path / "init")
    kept = run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, still)
    assert all(torch.equal(kept.state_dict()[k], v) for k, v in weights.items())

    lines: list[dict] = []
    moving = train_config(tmp_path / "moving", grad_accum=1, init_projector=tmp_path / "init")
    moved = run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, moving, lines.append)
    assert lines[0]["lr"] == pytest.approx(3e-2)
    assert any(not torch.equal(moved.state_dict()[k], v) for k, v in weights.items())
    assert load_file(tmp_path / "moving" / PROJECTOR_WEIGHTS).keys() == weights.keys()


def test_init_projector_with_another_config_is_refused(
    glimmer: FrozenGlimmer, tiny_encoder, audio_manifest: Path, tmp_path: Path
) -> None:
    save_run(tmp_path / "init", ProjectorConfig(DIM, 48, HIDDEN, 4))
    config = train_config(tmp_path / "out", init_projector=tmp_path / "init")
    with pytest.raises(ValueError, match="hidden_dim=48"):
        run_on(glimmer, tiny_encoder, audio_manifest, RECORDS, config)
    assert not (tmp_path / "out" / PROJECTOR_WEIGHTS).exists()


def test_cli_takes_the_projector_config_from_the_init_run_and_passes_history_flags(
    tiny_glimmer_dir: Path,
    tiny_whisper_dir: Path,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = tmp_path / "teacher.jsonl"
    manifest.write_text("".join(json.dumps(r.__dict__) + "\n" for r in RECORDS), encoding="utf-8")
    config = ProjectorConfig(DIM, 48, HIDDEN, 2)
    weights = save_run(tmp_path / "init", config)
    monkeypatch.setattr(train, "_load_tokenizer", lambda path: FakeTokenizer())
    batched: list = []
    real_make_batch = train.make_batch

    def spy_make_batch(*args, histories, **kwargs):
        batched.extend(histories)
        return real_make_batch(*args, histories=histories, **kwargs)

    monkeypatch.setattr(train, "make_batch", spy_make_batch)
    arguments = [
        *("--manifest", str(manifest)),
        *("--audio-manifest", str(audio_manifest)),
        *("--whisper-model", str(tiny_whisper_dir)),
        *("--glimmer", str(tiny_glimmer_dir)),
        *("--out-dir", str(tmp_path / "out")),
        *("--init-projector", str(tmp_path / "init")),
        *("--lr", "0"),
        *("--grad-accum", "1"),
        *("--history-prob", "1"),
        *("--history-turns", "1"),
    ]
    train.main(arguments)

    out = tmp_path / "out"
    assert json.loads((out / PROJECTOR_CONFIG).read_text()) == config.__dict__
    saved = load_file(out / PROJECTOR_WEIGHTS)
    assert all(torch.equal(saved[k], v) for k, v in weights.items())
    assert len(batched) == 2 and all(len(history) == 1 for history in batched)

    with pytest.raises(ValueError, match="hidden_dim"):
        train.main([*arguments, "--hidden-dim", "64"])


def test_cli_builds_the_augmenter_from_the_noise_manifest_and_probability(
    tiny_glimmer_dir: Path,
    tiny_whisper_dir: Path,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = tmp_path / "teacher.jsonl"
    manifest.write_text("".join(json.dumps(r.__dict__) + "\n" for r in RECORDS), encoding="utf-8")
    noise = write_noise_manifest(tmp_path / "noise", {"hiss": ("noise", 4000)})
    monkeypatch.setattr(train, "_load_tokenizer", lambda path: FakeTokenizer())
    built: list = []
    monkeypatch.setattr(train, "run", lambda *args: built.append(args[-1]))
    arguments = [
        *("--manifest", str(manifest)),
        *("--audio-manifest", str(audio_manifest)),
        *("--whisper-model", str(tiny_whisper_dir)),
        *("--glimmer", str(tiny_glimmer_dir)),
        *("--out-dir", str(tmp_path / "out")),
        *("--encoder-dim", str(DIM)),
        *("--seed", "4"),
    ]
    train.main(arguments)
    train.main([*arguments, *("--noise-manifest", str(noise)), *("--augment-prob", "0.25")])

    off, on = built
    assert off is None
    assert (on.prob, on.seed) == (0.25, 4)
