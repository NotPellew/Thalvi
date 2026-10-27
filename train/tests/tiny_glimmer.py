"""A tiny random Glimmer checkpoint and a fake tokenizer for CPU tests."""

from pathlib import Path

import torch
from transformers import MuseGlimmerConfig, MuseGlimmerForConditionalGeneration

from thalvi_train.glimmer import AUDIO_TOKEN_ID
from thalvi_train.prompts import AUDIO_TOKEN_TEXT

HIDDEN = 32
BOS = 200000
START = 200022
EOT = 200008


def save_tiny_glimmer(path: Path) -> MuseGlimmerForConditionalGeneration:
    """Saves a full tiny checkpoint whose logits are large enough to hit the softcap."""
    torch.manual_seed(0)
    config = MuseGlimmerConfig(
        text_config={
            "hidden_size": HIDDEN,
            "intermediate_size": 48,
            "num_hidden_layers": 4,
            "num_attention_heads": 4,
            "num_key_value_heads": 2,
            "head_dim": 8,
            "sliding_window": 8,
            "output_multiplier": 0.5,
            "final_logit_softcapping": 4.0,
        },
        vision_config={
            "hidden_size": 16,
            "intermediate_size": 24,
            "num_hidden_layers": 2,
            "num_attention_heads": 2,
        },
        out_hidden_size=16,
        projector_hidden_size=16,
    )
    model = MuseGlimmerForConditionalGeneration(config).eval()
    with torch.no_grad():
        model.lm_head.weight.mul_(100)
    model.save_pretrained(path)
    return model


class FakeTokenizer:
    """One id per character, the chat markers as single ids, `AUDIO_TOKEN_TEXT` as the audio id."""

    def chat_ids(self, system: str, user: str) -> list[int]:
        return [BOS, START, *self.text_ids(system), EOT, START, *self.text_ids(user), EOT]

    def text_ids(self, text: str) -> list[int]:
        ids: list[int] = []
        for i, part in enumerate(text.split(AUDIO_TOKEN_TEXT)):
            ids += [AUDIO_TOKEN_ID] * (i > 0) + [ord(c) for c in part]
        return ids

    def decode(self, ids: list[int]) -> str:
        specials = {START: "<|start|>", EOT: "<|eot|>"}
        return "".join(chr(i) if i < 128 else specials.get(i, f"<{i}>") for i in ids)
