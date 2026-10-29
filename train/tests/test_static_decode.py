from pathlib import Path

import pytest
import torch

from thalvi_train.decode import decode_steps
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.static_decode import (
    FORCED_RESERVE,
    STATIC_ATTENTION,
    StaticDecoder,
    fits,
    static_gqa_attention,
)

CAPACITY = 128
FORCED = [70, 71, 72]


@pytest.fixture
def glimmer(tiny_glimmer_dir: Path) -> FrozenGlimmer:
    return load_glimmer(tiny_glimmer_dir, dtype=torch.float32)


def prompt(length: int = 31) -> tuple[torch.Tensor, torch.Tensor]:
    ids = torch.tensor([[200000, *range(65, 65 + length - 2), 200022]])
    return ids, torch.ones_like(ids)


def run(glimmer: FrozenGlimmer, steps: int, forced_after: int | None = None) -> list[int]:
    ids, mask = prompt()
    gen = decode_steps(glimmer, glimmer.embed_text(ids), mask, steps, stop_id=-1)
    out = [next(gen)[0]]
    try:
        while True:
            out.append(gen.send(FORCED if len(out) == forced_after else None)[0])
    except StopIteration:
        return out


def test_fits_keeps_room_for_forced_chunks() -> None:
    assert fits(100, 50, 100 + 50 + FORCED_RESERVE)
    assert not fits(100, 51, 100 + 50 + FORCED_RESERVE)


@pytest.mark.parametrize("heads", [2, 4])
def test_one_token_attention_matches_stock_sdpa(heads: int) -> None:
    torch.manual_seed(0)
    query = torch.randn(1, 4, 1, 8)
    key, value = torch.randn(1, heads, 20, 8), torch.randn(1, heads, 20, 8)
    mask = (torch.arange(20) <= 12)[None, None, None]
    module = type("Module", (), {"num_key_value_groups": 4 // heads, "is_causal": True})()
    expected = torch.nn.functional.scaled_dot_product_attention(
        query,
        key.repeat_interleave(4 // heads, dim=1),
        value.repeat_interleave(4 // heads, dim=1),
        attn_mask=mask,
        scale=8**-0.5,
    ).transpose(1, 2)
    actual, _ = static_gqa_attention(module, query, key, value, mask, scaling=8**-0.5)
    assert torch.allclose(actual, expected, atol=1e-5)


@pytest.mark.parametrize("forced_after", [None, 5])
def test_static_decoding_matches_stock_past_the_sliding_window(
    glimmer: FrozenGlimmer, forced_after: int | None
) -> None:
    stock = run(glimmer, 50, forced_after)
    glimmer.static = StaticDecoder(glimmer, CAPACITY)
    assert run(glimmer, 50, forced_after) == stock


def test_static_run_is_used_and_released_when_closed(glimmer: FrozenGlimmer) -> None:
    glimmer.static = static = StaticDecoder(glimmer, CAPACITY)
    ids, mask = prompt()
    gen = decode_steps(glimmer, glimmer.embed_text(ids), mask, 5, stop_id=-1)
    next(gen)
    assert static.length == ids.shape[1]
    assert glimmer.decoder.config._attn_implementation == STATIC_ATTENTION
    gen.close()
    assert glimmer.decoder.config._attn_implementation != STATIC_ATTENTION
    assert static.acquire()


def stock_tokens(glimmer: FrozenGlimmer, ids: torch.Tensor, mask: torch.Tensor, steps: int):
    glimmer.static = None
    return [s[0] for s in decode_steps(glimmer, glimmer.embed_text(ids), mask, steps, stop_id=-1)]


def test_a_busy_decoder_sends_the_next_decode_to_the_stock_path(glimmer: FrozenGlimmer) -> None:
    ids, mask = prompt()
    expected = stock_tokens(glimmer, ids, mask, 6)
    glimmer.static = static = StaticDecoder(glimmer, CAPACITY)
    assert static.acquire()
    steps = decode_steps(glimmer, glimmer.embed_text(ids), mask, 6, stop_id=-1)
    assert [s[0] for s in steps] == expected
    assert static.length == 0


def test_a_padded_row_a_batch_or_a_long_request_use_the_stock_path(glimmer: FrozenGlimmer) -> None:
    glimmer.static = static = StaticDecoder(glimmer, CAPACITY)
    ids, mask = prompt()
    padded = torch.cat([torch.tensor([[200018]]), ids], dim=1)
    pad_mask = torch.cat([torch.zeros(1, 1, dtype=torch.long), mask], dim=1)
    cases = [
        (padded, pad_mask, 4),
        (ids.expand(2, -1), mask.expand(2, -1), 4),
        (ids, mask, CAPACITY),
    ]
    for case_ids, case_mask, steps in cases:
        list(decode_steps(glimmer, glimmer.embed_text(case_ids), case_mask, steps, stop_id=-1))
        assert static.length == 0
        assert static.acquire()
        static.release()


def test_feeding_past_the_capacity_raises(glimmer: FrozenGlimmer) -> None:
    static = StaticDecoder(glimmer, 8)
    static.acquire()
    with pytest.raises(ValueError, match="full"):
        static.feed_embeds(glimmer.embed_text(prompt()[0]))


@pytest.mark.skipif(not torch.cuda.is_available(), reason="needs a CUDA device")
@pytest.mark.parametrize("forced_after", [None, 5])
def test_captured_graph_matches_stock(tiny_glimmer_dir: Path, forced_after: int | None) -> None:
    glimmer = load_glimmer(tiny_glimmer_dir, device="cuda", dtype=torch.float32)
    stock = run(glimmer, 50, forced_after)
    glimmer.static = StaticDecoder(glimmer, CAPACITY)
    glimmer.static.warm_up()
    assert run(glimmer, 50, forced_after) == stock
