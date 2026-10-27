"""Token-by-token Glimmer decoding from a KV cache, shared by evaluation and serving.

Rows of a batch decode together, so a step yields one entry per row. Choosing
the next token is a `Pick`: greedy, or temperature and nucleus sampling. A caller
decoding one row can `send` tokens to feed in place of the one just chosen.
"""

from collections.abc import Callable, Generator

import torch

from thalvi_train.glimmer import PAD_TOKEN_ID, FrozenGlimmer

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
    mask = attention_mask.to(glimmer.device)
    rows = prompt_embeds.shape[0]
    hidden, cache = glimmer.cached_hidden_states(prompt_embeds, mask)
    token = pick(glimmer.logits(hidden[:, -1]))
    finished = [False] * rows
    for step in range(max_new_tokens):
        chosen = token.tolist()
        forced = yield [None if done else new for new, done in zip(chosen, finished, strict=True)]
        if forced:
            if rows != 1:
                raise ValueError("forced tokens need a single row")
            if step + 1 == max_new_tokens:
                return
            mask = torch.cat([mask, mask.new_ones(1, len(forced))], dim=1)
            fed = glimmer.embed_text(torch.tensor([forced]))
            hidden, cache = glimmer.cached_hidden_states(fed, mask, cache)
            token = pick(glimmer.logits(hidden[:, -1]))
            finished = [False]
            continue
        finished = [done or new == stop_id for new, done in zip(chosen, finished, strict=True)]
        if all(finished) or step + 1 == max_new_tokens:
            return
        fed = token.masked_fill(torch.tensor(finished, device=token.device), PAD_TOKEN_ID)
        mask = torch.cat([mask, mask.new_ones(rows, 1)], dim=1)
        hidden, cache = glimmer.cached_hidden_states(glimmer.embed_text(fed[:, None]), mask, cache)
        token = pick(glimmer.logits(hidden[:, -1]))
