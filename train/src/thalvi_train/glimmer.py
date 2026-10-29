"""Frozen Glimmer text decoder and lm_head: loading, audio splicing, cached decoding, and logits.

Only `model.language_model.*` and `lm_head.weight` are read from a full Glimmer
checkpoint; the vision tower is never built. The 4-bit branch
(`_load_decoder_4bit`, `_quantized_lm_head`) needs bitsandbytes and a CUDA
device, and is the only code that does.
"""

import json
from collections.abc import Sequence
from pathlib import Path
from typing import TYPE_CHECKING

import torch
from safetensors import safe_open
from torch import nn
from transformers import BitsAndBytesConfig, MuseGlimmerConfig
from transformers.cache_utils import Cache
from transformers.models.muse_glimmer.configuration_muse_glimmer import MuseGlimmerTextConfig
from transformers.models.muse_glimmer.modeling_muse_glimmer import MuseGlimmerTextModel

if TYPE_CHECKING:
    from thalvi_train.static_decode import StaticDecoder

AUDIO_TOKEN_ID = 201000
PAD_TOKEN_ID = 200018

LANGUAGE_MODEL_PREFIX = r"^model\.language_model\."
LM_HEAD_KEY = "lm_head.weight"
EMBED_TOKENS_KEY = "model.language_model.embed_tokens.weight"
LM_HEAD_SLICES = 8
IGNORED_CHECKPOINT_KEYS = ("model.vision_", LM_HEAD_KEY)


class FrozenGlimmer(nn.Module):
    """Glimmer's decoder and lm_head, frozen, with the audio splice and logit math.

    `embed_device` is where `embed_tokens` lives (CPU when the decoder is 4-bit
    on a GPU); everything else sits on `device`.
    """

    def __init__(
        self,
        decoder: MuseGlimmerTextModel,
        lm_head: nn.Module,
        device: torch.device,
        embed_device: torch.device,
    ) -> None:
        super().__init__()
        self.decoder = decoder
        self.lm_head = lm_head
        self.device = device
        self.embed_device = embed_device
        self.output_multiplier = decoder.config.output_multiplier
        self.softcap = decoder.config.final_logit_softcapping
        self.static: StaticDecoder | None = None
        self.requires_grad_(False)
        self.eval()

    @property
    def hidden_size(self) -> int:
        return self.decoder.config.hidden_size

    def enable_gradient_checkpointing(self) -> None:
        """Recomputes decoder layers in backward, so gradients reach the inputs cheaply.

        Checkpointing only runs in train mode; this is safe because the decoder
        has no dropout (`attention_dropout` is 0) and the weights stay frozen.
        """
        self.decoder.gradient_checkpointing_enable(
            gradient_checkpointing_kwargs={"use_reentrant": False}
        )
        self.decoder.train()

    def embed_text(self, input_ids: torch.Tensor) -> torch.Tensor:
        """Token embeddings (lookup plus the weightless norm) on `device`."""
        with torch.no_grad():
            embeds = self.decoder.embed_tokens(input_ids.to(self.embed_device))
        return embeds.to(self.device)

    def embed_with_audio(
        self, input_ids: torch.Tensor, audio: Sequence[torch.Tensor]
    ) -> torch.Tensor:
        """Token embeddings with `audio[i]` written over sample i's placeholder positions.

        `input_ids` is `(batch, length)` and may be padded on either side; `audio[i]` is
        `(tokens_i, hidden)` and `tokens_i` must equal the number of
        `AUDIO_TOKEN_ID`s in row i. Gradients flow into `audio`.
        """
        mask = input_ids.to(self.device) == AUDIO_TOKEN_ID
        placeholders = mask.sum(dim=1).tolist()
        supplied = [clip.shape[0] for clip in audio]
        if placeholders != supplied:
            raise ValueError(
                f"audio placeholders {placeholders} do not match audio tokens {supplied}"
            )
        embeds = self.embed_text(input_ids)
        flat = torch.cat(list(audio)).to(embeds.dtype)
        return embeds.masked_scatter(mask.unsqueeze(-1), flat)

    def hidden_states(
        self, inputs_embeds: torch.Tensor, attention_mask: torch.Tensor
    ) -> torch.Tensor:
        """Final-norm decoder states `(batch, length, hidden)`."""
        out = self.decoder(
            inputs_embeds=inputs_embeds, attention_mask=attention_mask, use_cache=False
        )
        return out.last_hidden_state

    def cached_hidden_states(
        self, inputs_embeds: torch.Tensor, attention_mask: torch.Tensor, cache: Cache | None = None
    ) -> tuple[torch.Tensor, Cache]:
        """Final-norm states of `inputs_embeds` `(batch, length, hidden)` appended to `cache`.

        `cache` is `None` for a prompt and the returned cache afterwards, which
        holds every position fed so far. `attention_mask` `(batch, seen + length)`
        marks the real positions of everything fed so far, this call included;
        rows may be left-padded. Each row's positions count its real tokens only,
        so padding shifts neither rotary positions nor the sliding window, which
        transformers measures in cache slots, the same distance for a left-padded row.
        """
        positions = (attention_mask.cumsum(dim=-1) - 1).clamp(min=0)[:, -inputs_embeds.shape[1] :]
        out = self.decoder(
            inputs_embeds=inputs_embeds,
            attention_mask=attention_mask,
            position_ids=positions,
            past_key_values=cache,
            use_cache=True,
        )
        return out.last_hidden_state, out.past_key_values

    def logits(self, hidden: torch.Tensor) -> torch.Tensor:
        """Vocabulary logits in fp32: `softcap * tanh(multiplier * head(hidden) / softcap)`."""
        raw = self.lm_head(hidden).float() * self.output_multiplier
        return torch.tanh(raw / self.softcap) * self.softcap


def load_glimmer(
    path: Path,
    *,
    device: str | torch.device = "cpu",
    dtype: torch.dtype = torch.bfloat16,
    quantize: bool = False,
) -> FrozenGlimmer:
    """Loads the decoder and lm_head of the Glimmer checkpoint at `path`, frozen.

    Raises if any decoder weight is missing from the checkpoint or any
    unexpected key is not a vision weight or the lm_head, since transformers
    would otherwise leave those weights randomly initialised.

    With `quantize`, decoder Linear layers and the lm_head load as NF4 with
    double quantization and bf16 compute, straight onto the GPU shard by shard,
    while `embed_tokens` stays on the CPU. The unquantized checkpoint is ~60 GB
    and loads unquantized straight onto `device`, e.g. an 80 GB GPU. Loading onto
    CUDA calls `avoid_cudnn_attention`.
    """
    device = torch.device(device)
    if device.type == "cuda":
        avoid_cudnn_attention()
    text_config = MuseGlimmerConfig.from_pretrained(path).text_config
    if quantize:
        decoder = _load_decoder_4bit(path, text_config, device)
        lm_head = _quantized_lm_head(path, text_config, device)
        embed_device = torch.device("cpu")
    else:
        decoder = _load_decoder(path, text_config, dtype, device)
        lm_head = _lm_head(path, text_config, dtype).to(device)
        embed_device = device
    return FrozenGlimmer(decoder, lm_head, device, embed_device)


def avoid_cudnn_attention() -> None:
    """Turns off cuDNN attention for the process, leaving flash, memory-efficient and math.

    PyTorch prefers it on Hopper GPUs, and it builds a graph for every new pair of
    query and key lengths: ~0.2 s each, against well under a millisecond for the
    others. Decoding meets a new key length every token, so a reply ran at ~1 token/s.
    """
    torch.backends.cuda.enable_cudnn_sdp(False)


def _load_decoder(
    path: Path, text_config: MuseGlimmerTextConfig, dtype: torch.dtype, device: torch.device
):
    decoder, info = MuseGlimmerTextModel.from_pretrained(
        path,
        config=text_config,
        key_mapping={LANGUAGE_MODEL_PREFIX: ""},
        dtype=dtype,
        device_map=device,
        output_loading_info=True,
    )
    _require_complete(info)
    return decoder


def _require_complete(info: dict[str, set[str]]) -> None:
    stray = {key for key in info["unexpected_keys"] if not key.startswith(IGNORED_CHECKPOINT_KEYS)}
    if info["missing_keys"] or stray or info["mismatched_keys"]:
        raise ValueError(
            f"decoder checkpoint mismatch: missing={sorted(info['missing_keys'])[:5]}, "
            f"unexpected={sorted(stray)[:5]}, mismatched={sorted(info['mismatched_keys'])[:5]}"
        )


def _read_tensor(path: Path, key: str) -> torch.Tensor:
    """Reads one tensor from whichever shard holds it, with `pread` rather than a mapping."""
    index = path / "model.safetensors.index.json"
    shard = json.loads(index.read_text())["weight_map"][key] if index.exists() else None
    with safe_open(
        path / (shard or "model.safetensors"), framework="pt", backend="pread"
    ) as shards:
        return shards.get_tensor(key)


def _lm_head(path: Path, text_config: MuseGlimmerTextConfig, dtype: torch.dtype) -> nn.Linear:
    weight = _read_tensor(path, LM_HEAD_KEY).to(dtype)
    head = nn.Linear(text_config.hidden_size, text_config.vocab_size, bias=False, device="meta")
    head.weight = nn.Parameter(weight, requires_grad=False)
    return head


def _load_decoder_4bit(path: Path, text_config: MuseGlimmerTextConfig, device: torch.device):
    quantization = BitsAndBytesConfig(
        load_in_4bit=True,
        bnb_4bit_quant_type="nf4",
        bnb_4bit_compute_dtype=torch.bfloat16,
        bnb_4bit_use_double_quant=True,
        llm_int8_enable_fp32_cpu_offload=True,
    )
    device_map = {
        "embed_tokens": "cpu",
        "layers": device.index or 0,
        "norm": device.index or 0,
        "rotary_emb": device.index or 0,
    }
    decoder, info = MuseGlimmerTextModel.from_pretrained(
        path,
        config=text_config,
        key_mapping={LANGUAGE_MODEL_PREFIX: ""},
        dtype=torch.bfloat16,
        quantization_config=quantization,
        device_map=device_map,
        output_loading_info=True,
    )
    _require_complete(info)
    _keep_embeddings_on_cpu(decoder, path)
    return decoder


def _keep_embeddings_on_cpu(decoder: MuseGlimmerTextModel, path: Path) -> None:
    """Gives `embed_tokens` its weights on the CPU, without accelerate's offload hook.

    With the hook, every lookup would copy the 2.7 GB table to the GPU first.
    """
    from accelerate.hooks import remove_hook_from_module

    remove_hook_from_module(decoder.embed_tokens)
    weight = _read_tensor(path, EMBED_TOKENS_KEY).to(torch.bfloat16)
    decoder.embed_tokens.weight = nn.Parameter(weight, requires_grad=False)


class ChunkedHead(nn.Module):
    """A Linear split by output rows, so each slice quantizes with little GPU memory."""

    def __init__(self, parts: Sequence[nn.Module]) -> None:
        super().__init__()
        self.parts = nn.ModuleList(parts)

    def forward(self, hidden: torch.Tensor) -> torch.Tensor:
        return torch.cat([part(hidden) for part in self.parts], dim=-1)


def _quantized_lm_head(path: Path, text_config: MuseGlimmerTextConfig, device: torch.device):
    """NF4 lm_head in `LM_HEAD_SLICES` row slices.

    bitsandbytes quantizes on the GPU from a bf16 copy, and the whole 2.5 GB
    matrix does not fit next to the 4-bit decoder on a 16 GB card.
    """
    import bitsandbytes as bnb

    weight = _read_tensor(path, LM_HEAD_KEY).to(torch.bfloat16)
    parts = []
    for rows in weight.chunk(LM_HEAD_SLICES, dim=0):
        part = bnb.nn.Linear4bit(
            text_config.hidden_size,
            rows.shape[0],
            bias=False,
            compute_dtype=torch.bfloat16,
            compress_statistics=True,
            quant_type="nf4",
            device="meta",
        )
        part.weight = bnb.nn.Params4bit(
            rows.contiguous(), requires_grad=False, compress_statistics=True, quant_type="nf4"
        )
        parts.append(part.to(device))
    return ChunkedHead(parts)
