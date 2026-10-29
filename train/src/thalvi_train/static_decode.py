"""Single-row Glimmer decoding from a preallocated KV cache, one CUDA graph per token.

Decoding a token through the stock path launches hundreds of small kernels from
Python and rebuilds the attention masks each step, so the GPU idles on the CPU.
Here the K/V buffers have a fixed capacity, a one-token step reads its position from a
device tensor, and the whole step (embedding, 52 layers, masks, lm_head and softcap)
is captured once as a CUDA graph and replayed. A prompt or a forced chunk of several
tokens runs eagerly into the same buffers, with masks built by hand.

`compile=True` first fuses the step's elementwise kernels with `torch.compile` (~1.5 minutes
at load, ~30% faster); the matmuls stay cuBLAS. Greedy tokens agree with the stock path up to
bf16 noise: near-tied tokens can flip, as they do under any other attention kernel.

A `StaticDecoder` serves one decode at a time (`acquire`/`release`); `decode_steps`
falls back to the stock path when it is busy, when the row is padded or batched, or when
the request does not fit `capacity`. While acquired, the decoder's attention is
`static_gqa`: a one-token query attends with its query-head groups as the query
dimension, so grouped K/V need no copy, and anything longer goes to stock SDPA; stock
masks are built as for SDPA, so a decode on the stock path meanwhile is unaffected.
"""

import torch
from torch import nn
from transformers import AttentionInterface
from transformers.integrations.sdpa_attention import sdpa_attention_forward
from transformers.masking_utils import AttentionMaskInterface, sdpa_mask

from thalvi_train.glimmer import FrozenGlimmer

DEFAULT_CAPACITY = 8192
FORCED_RESERVE = 64
STATIC_ATTENTION = "static_gqa"
WARMUP_STEPS = 3


def fits(prompt_tokens: int, max_new_tokens: int, capacity: int) -> bool:
    """Whether a decode of `max_new_tokens` after `prompt_tokens` fits the buffers.

    `FORCED_RESERVE` positions stay free for forced chunks, which are fed in place
    of a token that was not.
    """
    return prompt_tokens + max_new_tokens + FORCED_RESERVE <= capacity


def _float_scores(grouped: torch.Tensor, key: torch.Tensor) -> torch.Tensor:
    """`grouped @ key^T` in fp32, accumulated and kept so, as the fused kernels do.

    A bf16 result would round every score to 8 bits before the softmax, enough to flip
    near-tied greedy tokens.
    """
    batch, kv_heads, groups, head_dim = grouped.shape
    flat = grouped.reshape(batch * kv_heads, groups, head_dim)
    keys = key.reshape(batch * kv_heads, -1, head_dim).transpose(1, 2)
    if grouped.dtype == torch.float32:
        scores = torch.bmm(flat, keys)
    else:
        scores = torch.bmm(flat, keys, out_dtype=torch.float32)
    return scores.view(batch, kv_heads, groups, -1)


def static_gqa_attention(
    module: nn.Module,
    query: torch.Tensor,
    key: torch.Tensor,
    value: torch.Tensor,
    attention_mask: torch.Tensor | None,
    **kwargs,
) -> tuple[torch.Tensor, None]:
    """SDPA that, for one query token, folds the query heads of a K/V head into the query axis.

    The fused kernels run that shape on two K/V heads with almost no parallelism, ~340 us
    a layer over 8192 keys; two batched matmuls around a softmax take ~30 us.
    """
    if query.shape[2] != 1:
        return sdpa_attention_forward(module, query, key, value, attention_mask, **kwargs)
    batch, heads, _, head_dim = query.shape
    kv_heads = key.shape[1]
    grouped = query.reshape(batch, kv_heads, heads // kv_heads, head_dim)
    scores = _float_scores(grouped, key) * kwargs["scaling"]
    if attention_mask is not None:
        scores = scores.masked_fill(~attention_mask, float("-inf"))
    weights = torch.softmax(scores, dim=-1, dtype=torch.float32).to(query.dtype)
    out = torch.matmul(weights, value)
    return out.reshape(batch, 1, heads, head_dim), None


AttentionInterface.register(STATIC_ATTENTION, static_gqa_attention)
AttentionMaskInterface.register(STATIC_ATTENTION, sdpa_mask)


class StaticKV:
    """Fixed-capacity K/V buffers of one row, written in place at the current length.

    Eagerly, `update` writes at the host-side `length` and returns the filled prefix;
    the caller advances `length` afterwards. In `graph_mode` it writes at the position
    held in the device tensor `pos` and returns the whole buffers.
    """

    def __init__(self, glimmer: FrozenGlimmer, capacity: int) -> None:
        config = glimmer.decoder.config
        weight = next(glimmer.decoder.parameters())
        shape = (1, config.num_key_value_heads, capacity, config.head_dim)
        self.keys = [weight.new_zeros(shape) for _ in range(config.num_hidden_layers)]
        self.values = [weight.new_zeros(shape) for _ in range(config.num_hidden_layers)]
        self.capacity = capacity
        self.length = 0
        self.pos = torch.zeros(1, dtype=torch.long, device=weight.device)
        self.graph_mode = False

    def update(
        self,
        key_states: torch.Tensor,
        value_states: torch.Tensor,
        layer_idx: int,
        cache_kwargs: dict | None = None,
    ) -> tuple[torch.Tensor, torch.Tensor]:
        keys, values = self.keys[layer_idx], self.values[layer_idx]
        if self.graph_mode:
            keys.index_copy_(2, self.pos, key_states)
            values.index_copy_(2, self.pos, value_states)
            return keys, values
        end = self.length + key_states.shape[2]
        keys[:, :, self.length : end] = key_states
        values[:, :, self.length : end] = value_states
        return keys[:, :, :end], values[:, :, :end]


class StaticDecoder:
    """The static cache, the captured one-token step, and the eager path into the same buffers."""

    def __init__(
        self, glimmer: FrozenGlimmer, capacity: int = DEFAULT_CAPACITY, *, compile: bool = False
    ) -> None:
        if glimmer.embed_device != glimmer.device:
            raise ValueError("static decoding needs embed_tokens on the model's device")
        self.glimmer = glimmer
        self.capacity = capacity
        self.cache = StaticKV(glimmer, capacity)
        self._config = glimmer.decoder.config
        self._ids = torch.zeros(1, 1, dtype=torch.long, device=glimmer.device)
        self._logits: torch.Tensor | None = None
        self._graph: torch.cuda.CUDAGraph | None = None
        self._body = (
            torch.compile(self._graph_body, fullgraph=True) if compile else self._graph_body
        )
        self._busy = False
        self._restore = self._config._attn_implementation

    @property
    def length(self) -> int:
        """Positions fed so far."""
        return self.cache.length

    def acquire(self) -> bool:
        """Claims the decoder for one decode; `False` if another holds it."""
        if self._busy:
            return False
        self._busy = True
        self._restore = self._config._attn_implementation
        self._config._attn_implementation = STATIC_ATTENTION
        self.cache.length = 0
        return True

    def release(self) -> None:
        """Gives the decoder back and restores the stock attention."""
        self._config._attn_implementation = self._restore
        self._busy = False

    def warm_up(self) -> None:
        """Captures the one-token step; run once at load, before any request. Needs CUDA.

        Without a capture, one-token steps run eagerly into the same buffers.
        """
        if self.glimmer.device.type != "cuda":
            raise ValueError("CUDA graphs need a CUDA device")
        if not self.acquire():
            raise RuntimeError("static decoder is busy")
        try:
            self.cache.graph_mode = True
            stream = torch.cuda.Stream()
            stream.wait_stream(torch.cuda.current_stream())
            with torch.cuda.stream(stream), torch.no_grad():
                for _ in range(WARMUP_STEPS):
                    self._body()
            torch.cuda.current_stream().wait_stream(stream)
            graph = torch.cuda.CUDAGraph()
            with torch.cuda.graph(graph), torch.no_grad():
                self._logits = self._body()
            self._graph = graph
            torch.cuda.synchronize()
        finally:
            self.cache.graph_mode = False
            self.release()

    def feed(self, ids: torch.Tensor) -> torch.Tensor:
        """Appends `ids` `(1, n)` and returns the logits `(1, vocab)` after the last one.

        A one-token step returns the graph's output buffer, which the next step overwrites.
        """
        n = ids.shape[1]
        if self.length + n > self.capacity:
            raise ValueError(f"static cache of {self.capacity} positions is full")
        if n == 1 and self._graph is not None:
            self._ids.copy_(ids)
            self.cache.pos.fill_(self.length)
            self._graph.replay()
            self.cache.length += 1
            assert self._logits is not None
            return self._logits
        return self._feed_eager(self.glimmer.embed_text(ids))

    def feed_embeds(self, embeds: torch.Tensor) -> torch.Tensor:
        """Appends prompt `embeds` `(1, n, hidden)` eagerly; returns the last position's logits."""
        if self.length + embeds.shape[1] > self.capacity:
            raise ValueError(f"static cache of {self.capacity} positions is full")
        return self._feed_eager(embeds)

    def _feed_eager(self, embeds: torch.Tensor) -> torch.Tensor:
        n, start = embeds.shape[1], self.length
        positions = torch.arange(start, start + n, device=embeds.device)
        keys = torch.arange(start + n, device=embeds.device)
        causal = keys[None, :] <= positions[:, None]
        window = keys[None, :] > positions[:, None] - self._config.sliding_window
        masks = {
            "full_attention": causal[None, None],
            "sliding_attention": (causal & window)[None, None],
        }
        out = self.glimmer.decoder(
            inputs_embeds=embeds,
            attention_mask=masks,
            position_ids=positions[None],
            past_key_values=self.cache,
            use_cache=True,
        )
        self.cache.length += n
        return self.glimmer.logits(out.last_hidden_state[:, -1])

    def _graph_body(self) -> torch.Tensor:
        """The one-token step of token `_ids` at position `cache.pos`, all in static tensors."""
        pos = self.cache.pos
        keys = torch.arange(self.capacity, device=pos.device)
        causal = keys <= pos
        window = keys > pos - self._config.sliding_window
        masks = {
            "full_attention": causal[None, None, None],
            "sliding_attention": (causal & window)[None, None, None],
        }
        out = self.glimmer.decoder(
            inputs_embeds=self.glimmer.decoder.embed_tokens(self._ids),
            attention_mask=masks,
            position_ids=pos[None],
            past_key_values=self.cache,
            use_cache=True,
        )
        return self.glimmer.logits(out.last_hidden_state[:, -1])
