"""Token-by-token Glimmer decoding from a KV cache, shared by evaluation and serving.

Rows of a batch decode together, so a step yields one entry per row. Choosing
the next token is a `Pick`: greedy, or temperature and nucleus sampling. A caller
decoding one row can `send` tokens to feed in place of the one just chosen.

A glimmer with a `StaticDecoder` (`FrozenGlimmer.static`) decodes an unpadded single
row from its CUDA graph instead, when the request fits; greedy tokens match up to
bf16 noise, which can flip a near-tied choice.
"""

from collections.abc import Callable, Generator

import torch

from thalvi_train.glimmer import PAD_TOKEN_ID, FrozenGlimmer
from thalvi_train.static_decode import StaticDecoder, fits

Pick = Callable[[torch.Tensor], torch.Tensor]


def greedy(logits: torch.Tensor) -> torch.Tensor:
    """The most likely token of each row of `logits` `(batch, vocab)`."""
    return logits.argmax(dim=-1)


def sampler(temperature: float, top_p: float = 1.0) -> Pick:
    """A `Pick` that samples from the softmax at `temperature`, within the `top_p` nucleus.

    A non-positive temperature is greedy. The nucleus is the fewest of the most
    likely tokens whose probabilities reach `top_p`; the likeliest always stays.
    """
    if temperature <= 0:
        return greedy

    def pick(logits: torch.Tensor) -> torch.Tensor:
        probs = torch.softmax(logits / temperature, dim=-1)
        if top_p < 1.0:
            ordered, order = probs.sort(dim=-1, descending=True)
            before = ordered.cumsum(dim=-1) - ordered
            ordered = ordered.masked_fill(before >= top_p, 0.0)
            probs = torch.zeros_like(probs).scatter(-1, order, ordered)
        return torch.multinomial(probs, 1).squeeze(-1)

    return pick


class _StockRun:
    """The row batch's growing mask and `DynamicCache`, fed by the stock transformers path."""

    def __init__(self, glimmer: FrozenGlimmer, mask: torch.Tensor) -> None:
        self._glimmer = glimmer
        self._mask = mask.to(glimmer.device)
        self._cache = None

    def start(self, embeds: torch.Tensor) -> torch.Tensor:
        hidden, self._cache = self._glimmer.cached_hidden_states(embeds, self._mask)
        return self._glimmer.logits(hidden[:, -1])

    def feed(self, ids: torch.Tensor) -> torch.Tensor:
        rows, count = ids.shape
        self._mask = torch.cat([self._mask, self._mask.new_ones(rows, count)], dim=1)
        fed = self._glimmer.embed_text(ids)
        hidden, self._cache = self._glimmer.cached_hidden_states(fed, self._mask, self._cache)
        return self._glimmer.logits(hidden[:, -1])

    def close(self) -> None:
        self._cache = None


class _StaticRun:
    """A claimed `StaticDecoder`, released when the decode closes."""

    def __init__(self, static: StaticDecoder) -> None:
        self._static = static

    def start(self, embeds: torch.Tensor) -> torch.Tensor:
        return self._static.feed_embeds(embeds)

    def feed(self, ids: torch.Tensor) -> torch.Tensor:
        return self._static.feed(ids)

    def close(self) -> None:
        self._static.release()


def open_run(
    glimmer: FrozenGlimmer, prompt_embeds: torch.Tensor, mask: torch.Tensor, max_new_tokens: int
) -> _StockRun | _StaticRun:
    """The static run if `glimmer` has a free decoder fitting the request, else the stock one."""
    static = glimmer.static
    if (
        static is not None
        and prompt_embeds.shape[0] == 1
        and fits(prompt_embeds.shape[1], max_new_tokens, static.capacity)
        and bool(mask.all())
        and static.acquire()
    ):
        return _StaticRun(static)
    return _StockRun(glimmer, mask)


@torch.no_grad()
def decode_steps(
    glimmer: FrozenGlimmer,
    prompt_embeds: torch.Tensor,
    attention_mask: torch.Tensor,
    max_new_tokens: int,
    stop_id: int,
    pick: Pick = greedy,
) -> Generator[list[int | None], list[int] | None, None]:
    """Decodes from left-padded `prompt_embeds` `(batch, length, hidden)`, one step per `next`.

    `attention_mask` `(batch, length)` is 0 on the padding. A step is the token
    each row chose, or `None` for a row that already emitted `stop_id`; such a
    row is fed pad tokens meanwhile. The first step comes out of the prompt's
    prefill. The generator ends once every row has stopped or `max_new_tokens`
    steps were made; closing it early frees the cache.

    A single-row decode can `send` a list of token ids after a step: the token
    that step chose is dropped unfed, the ids go into the same cache in one pass,
    and the next step is chosen after them, even if the dropped token was
    `stop_id`. The dropped step still counts toward `max_new_tokens`, and the
    generator ends instead of yielding if that was the last one.
    """
    if max_new_tokens < 1:
        raise ValueError("max_new_tokens must be at least 1")
    rows = prompt_embeds.shape[0]
    run = open_run(glimmer, prompt_embeds, attention_mask, max_new_tokens)
    try:
        token = pick(run.start(prompt_embeds))
        finished = [False] * rows
        for step in range(max_new_tokens):
            chosen = token.tolist()
            forced = yield [
                None if done else new for new, done in zip(chosen, finished, strict=True)
            ]
            if forced:
                if rows != 1:
                    raise ValueError("forced tokens need a single row")
                if step + 1 == max_new_tokens:
                    return
                token = pick(run.feed(torch.tensor([forced])))
                finished = [False]
                continue
            finished = [done or new == stop_id for new, done in zip(chosen, finished, strict=True)]
            if all(finished) or step + 1 == max_new_tokens:
                return
            if any(finished):
                token = token.masked_fill(torch.tensor(finished, device=token.device), PAD_TOKEN_ID)
            token = pick(run.feed(token[:, None]))
    finally:
        run.close()
