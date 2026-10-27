import pytest
import torch

from thalvi_train.projector import AudioProjector


def small_projector() -> AudioProjector:
    return AudioProjector(encoder_dim=16, hidden_dim=32, text_dim=24, stack=4)


@pytest.mark.parametrize(("frames", "tokens"), [(1, 1), (4, 1), (5, 2), (12, 3)])
def test_stacks_frames_into_the_announced_number_of_tokens(frames: int, tokens: int) -> None:
    projector = small_projector()
    out = projector(torch.randn(2, frames, 16))
    assert out.shape == (2, tokens, 24)
    assert projector.audio_token_count(frames) == tokens


def test_outputs_unit_rms_embeddings_like_glimmer_tokens() -> None:
    out = small_projector()(torch.randn(3, 9, 16) * 50)
    rms = out.pow(2).mean(dim=-1).sqrt()
    assert torch.allclose(rms, torch.ones_like(rms), atol=1e-2)


def test_trains_only_its_own_weights() -> None:
    projector = small_projector()
    projector(torch.randn(1, 8, 16)).sum().backward()
    assert all(p.grad is not None for p in projector.parameters())
