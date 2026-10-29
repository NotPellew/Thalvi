import shutil
from pathlib import Path

import pytest
import torch
from safetensors.torch import load_file, save_file
from tiny_glimmer import HIDDEN
from transformers import MuseGlimmerForConditionalGeneration

from thalvi_train.glimmer import AUDIO_TOKEN_ID, FrozenGlimmer, load_glimmer


@pytest.fixture
def glimmer(tiny_glimmer_dir: Path) -> FrozenGlimmer:
    return load_glimmer(tiny_glimmer_dir, dtype=torch.float32)


def test_loads_only_frozen_text_weights_identical_to_the_checkpoint(
    glimmer: FrozenGlimmer, reference_model: MuseGlimmerForConditionalGeneration
) -> None:
    expected = reference_model.model.language_model.state_dict()
    assert glimmer.decoder.state_dict().keys() == expected.keys()
    assert all(torch.equal(v, expected[k]) for k, v in glimmer.decoder.state_dict().items())
    assert torch.equal(glimmer.lm_head.weight, reference_model.lm_head.weight)
    assert not any(p.requires_grad for p in glimmer.parameters())
    assert not glimmer.training
    assert not any("vision" in name for name, _ in glimmer.named_modules())


def test_refuses_a_checkpoint_missing_decoder_weights(
    tiny_glimmer_dir: Path, tmp_path: Path
) -> None:
    shutil.copytree(tiny_glimmer_dir, tmp_path, dirs_exist_ok=True)
    weights = load_file(tmp_path / "model.safetensors")
    del weights["model.language_model.norm.weight"]
    save_file(weights, tmp_path / "model.safetensors", metadata={"format": "pt"})
    with pytest.raises(ValueError, match=r"norm\.weight"):
        load_glimmer(tmp_path, dtype=torch.float32)


def test_logits_match_the_full_model(
    glimmer: FrozenGlimmer, reference_model: MuseGlimmerForConditionalGeneration
) -> None:
    ids = torch.tensor([[5, 17, 9000, 201999, 3, 44], [8, 8, 8, 100, 0, 7]])
    mask = torch.ones_like(ids)
    with torch.no_grad():
        ours = glimmer.logits(glimmer.hidden_states(glimmer.embed_text(ids), mask))
        theirs = reference_model(input_ids=ids, attention_mask=mask).logits
    assert ours.abs().max() > 0.9 * glimmer.softcap
    assert torch.allclose(ours, theirs, atol=1e-5)


def test_splice_writes_each_samples_audio_at_its_placeholders_only(glimmer: FrozenGlimmer) -> None:
    p = AUDIO_TOKEN_ID
    ids = torch.tensor([[1, p, p, 2, 0], [3, p, 4, 0, 0]])
    audio = [torch.randn(2, HIDDEN), torch.randn(1, HIDDEN)]
    plain = glimmer.embed_text(ids)
    spliced = glimmer.embed_with_audio(ids, audio)
    assert torch.equal(spliced[0, 1:3], audio[0])
    assert torch.equal(spliced[1, 1], audio[1][0])
    untouched = ids != p
    assert torch.equal(spliced[untouched], plain[untouched])


@pytest.mark.parametrize("audio_tokens", [[1, 1], [2], [2, 1, 1]])
def test_splice_rejects_audio_that_does_not_fill_the_placeholders(
    glimmer: FrozenGlimmer, audio_tokens: list[int]
) -> None:
    p = AUDIO_TOKEN_ID
    ids = torch.tensor([[1, p, p, 2], [3, p, 4, 0]])
    with pytest.raises(ValueError, match="placeholders"):
        glimmer.embed_with_audio(ids, [torch.randn(n, HIDDEN) for n in audio_tokens])


def test_cudnn_attention_is_turned_off_for_the_process() -> None:
    """Guards a reply decoding at ~1 token/s on H100s: cuDNN builds a graph per new length."""
    import torch

    from thalvi_train.glimmer import avoid_cudnn_attention

    before = torch.backends.cuda.cudnn_sdp_enabled()
    try:
        avoid_cudnn_attention()
        assert not torch.backends.cuda.cudnn_sdp_enabled()
    finally:
        torch.backends.cuda.enable_cudnn_sdp(before)
