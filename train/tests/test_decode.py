from pathlib import Path

import pytest
import torch

from thalvi_train.decode import decode_steps, greedy, sampler
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer


@pytest.fixture(scope="module")
def glimmer(tiny_glimmer_dir: Path) -> FrozenGlimmer:
    return load_glimmer(tiny_glimmer_dir, dtype=torch.float32)


def draws(pick, logits: list[float], count: int = 300) -> set[int]:
    torch.manual_seed(0)
    row = torch.tensor([logits]).expand(count, -1)
    return set(pick(row).tolist())


def test_zero_temperature_is_greedy() -> None:
    assert sampler(0.0, 0.5) is greedy
    assert greedy(torch.tensor([[0.0, 3.0, 1.0], [2.0, 0.0, 1.0]])).tolist() == [1, 0]


@pytest.mark.parametrize(
    ("logits", "top_p", "temperature", "reachable"),
    [
        ([0.0, 0.0], 1.0, 1.0, {0, 1}),
        ([1.0, 5.0, 2.0], 0.1, 1.0, {1}),
        ([0.0, 0.0, -50.0], 1.0, 1.0, {0, 1}),
        ([2.0, 1.9, 1.8, -9.0], 0.5, 1.0, {0, 1}),
        ([1.0, 5.0, 2.0], 1.0, 0.01, {1}),
    ],
)
def test_sampling_stays_within_the_temperature_scaled_nucleus(
    logits: list[float], top_p: float, temperature: float, reachable: set[int]
) -> None:
    assert draws(sampler(temperature, top_p), logits) == reachable


def prompt(glimmer: FrozenGlimmer) -> tuple[torch.Tensor, torch.Tensor]:
    ids = torch.tensor([[200000, 65, 66, 67, 200022]])
    return glimmer.embed_text(ids), torch.ones_like(ids)


def test_steps_end_at_the_stop_token_or_the_limit(glimmer: FrozenGlimmer) -> None:
    embeds, mask = prompt(glimmer)
    unbounded = [step[0] for step in decode_steps(glimmer, embeds, mask, 4, stop_id=-1)]
    assert len(unbounded) == 4
    stopped = [step[0] for step in decode_steps(glimmer, embeds, mask, 4, stop_id=unbounded[1])]
    assert stopped == unbounded[: stopped.index(unbounded[1]) + 1]


def test_a_stopped_row_yields_none_while_others_go_on(glimmer: FrozenGlimmer) -> None:
    ids = torch.tensor([[200000, 65, 66, 67, 200022], [200000, 70, 71, 72, 200022]])
    embeds, mask = glimmer.embed_text(ids), torch.ones_like(ids)
    free = list(decode_steps(glimmer, embeds, mask, 6, stop_id=-1))
    stop = free[0][0]
    cut = list(decode_steps(glimmer, embeds, mask, 6, stop_id=stop))
    assert cut[0] == free[0]
    assert all(step[0] is None for step in cut[1:])
    assert free[0][1] != stop
    assert len(cut) > 1
    assert cut[1][1] == free[1][1]


def test_rejects_a_zero_limit(glimmer: FrozenGlimmer) -> None:
    embeds, mask = prompt(glimmer)
    with pytest.raises(ValueError, match="at least 1"):
        next(decode_steps(glimmer, embeds, mask, 0, stop_id=1))


FORCED = [70, 71, 72, 73, 74, 75, 76]


def logits_after(glimmer: FrozenGlimmer, ids: list[int]) -> torch.Tensor:
    embeds = glimmer.embed_text(torch.tensor([ids]))
    return glimmer.logits(glimmer.hidden_states(embeds, torch.ones(1, len(ids), dtype=torch.long)))[
        :, -1
    ]


def test_forced_tokens_continue_the_cache_like_one_prefill_of_the_whole_sequence(
    glimmer: FrozenGlimmer,
) -> None:
    ids = [200000, 65, 66, 67, 200022]
    embeds, mask = prompt(glimmer)
    picked: list[torch.Tensor] = []

    def recording(logits: torch.Tensor) -> torch.Tensor:
        picked.append(logits)
        return greedy(logits)

    steps = decode_steps(glimmer, embeds, mask, 6, stop_id=-1, pick=recording)
    next(steps)
    (after,) = steps.send(FORCED)
    torch.testing.assert_close(picked[-1], logits_after(glimmer, ids + FORCED), rtol=0, atol=1e-4)
    assert after == picked[-1].argmax(dim=-1).item()
    next(steps)
    torch.testing.assert_close(
        picked[-1], logits_after(glimmer, [*ids, *FORCED, after]), rtol=0, atol=1e-4
    )


def test_forced_tokens_replace_a_stop_token_but_not_its_place_in_the_limit(
    glimmer: FrozenGlimmer,
) -> None:
    embeds, mask = prompt(glimmer)
    (first,) = next(decode_steps(glimmer, embeds, mask, 3, stop_id=-1))
    steps = decode_steps(glimmer, embeds, mask, 3, stop_id=first)
    assert next(steps) == [first]
    assert steps.send(FORCED) is not None
    assert next(steps, None) is not None
    assert next(steps, None) is None
    last = decode_steps(glimmer, embeds, mask, 1, stop_id=-1)
    next(last)
    with pytest.raises(StopIteration):
        last.send(FORCED)


def test_forced_tokens_need_a_single_row(glimmer: FrozenGlimmer) -> None:
    ids = torch.tensor([[200000, 65, 66, 67, 200022]] * 2)
    steps = decode_steps(glimmer, glimmer.embed_text(ids), torch.ones_like(ids), 3, stop_id=-1)
    next(steps)
    with pytest.raises(ValueError, match="single row"):
        steps.send(FORCED)
