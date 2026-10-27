"""Runs one chat request through the audio encoder, projector and Glimmer, one at a time.

The engine owns the loaded models and a lock, so requests queue and generate in turn.
`Engine.run` is blocking and reports through `emit`: `Ready` once the first token is
chosen, then a `Step` per token with the events it completed, then `Finished`.
Setting `cancelled` stops it before the next token.
"""

import threading
from collections.abc import Callable
from dataclasses import dataclass
from typing import Literal

import torch
from transformers import WhisperFeatureExtractor
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.chat import ChatRequest, prompt_text
from thalvi_train.decode import decode_steps, sampler
from thalvi_train.features import encode_long
from thalvi_train.glimmer import FrozenGlimmer
from thalvi_train.projector import AudioProjector
from thalvi_train.prompts import ChatTokenizer
from thalvi_train.reply import (
    Event,
    IncrementalDecoder,
    ReplyParser,
    ToolCall,
    tool_parameter_types,
)
from thalvi_train.teacher import EOT_ID

FinishReason = Literal["stop", "length", "tool_calls"]


@dataclass(frozen=True)
class Ready:
    """The prompt was built and prefilled, and the first token chosen."""


@dataclass(frozen=True)
class Step:
    """The events one generated token completed; often none."""

    events: list[Event]


@dataclass(frozen=True)
class Finished:
    """Generation ended: why, and the token counts."""

    reason: FinishReason
    prompt_tokens: int
    completion_tokens: int


Update = Ready | Step | Finished


class Engine:
    """Glimmer with its hearing: encoder, projector, decoder and tokenizer."""

    def __init__(
        self,
        glimmer: FrozenGlimmer,
        tokenizer: ChatTokenizer,
        projector: AudioProjector,
        encoder: WhisperEncoder,
        extractor: WhisperFeatureExtractor,
    ) -> None:
        self._glimmer = glimmer
        self._tokenizer = tokenizer
        self._projector = projector
        self._encoder = encoder
        self._extractor = extractor
        self._lock = threading.Lock()

    def run(
        self, request: ChatRequest, emit: Callable[[Update], None], cancelled: threading.Event
    ) -> None:
        """Generates the reply to `request`, waiting for any earlier request to finish first.

        Returns without a `Finished` if `cancelled` is set. Errors propagate; if they
        come before `Ready`, nothing was emitted.
        """
        device = self._glimmer.device
        autocast = torch.autocast(device.type, dtype=torch.bfloat16, enabled=device.type == "cuda")
        with self._lock, torch.no_grad(), autocast:
            if cancelled.is_set():
                return
            embeds, prompt_tokens = self._prompt(request)
            parser = ReplyParser(tool_parameter_types(request.tools))
            decoder = IncrementalDecoder(self._tokenizer.decode)
            pick = sampler(request.temperature, request.top_p)
            steps = decode_steps(
                self._glimmer,
                embeds,
                embeds.new_ones(embeds.shape[:2], dtype=torch.long),
                request.max_tokens,
                EOT_ID,
                pick,
            )
            generated, called = 0, False
            try:
                for (token,) in steps:
                    if cancelled.is_set():
                        return
                    if generated == 0:
                        emit(Ready())
                    generated += 1
                    events = parser.feed(decoder.push(token))
                    called = called or any(isinstance(e, ToolCall) for e in events)
                    emit(Step(events))
                    if parser.done:
                        break
            finally:
                steps.close()
            events = parser.feed(decoder.flush()) + parser.finish()
            if events:
                called = called or any(isinstance(e, ToolCall) for e in events)
                emit(Step(events))
            reason: FinishReason = "tool_calls" if called else "stop"
            if not parser.done:
                reason = "length"
            emit(Finished(reason, prompt_tokens, generated))

    def _prompt(self, request: ChatRequest) -> tuple[torch.Tensor, int]:
        """The prompt's embeddings `(1, length, hidden)`, clips spliced in, and its length."""
        weights = next(self._projector.parameters())
        audio = [
            self._projector(
                encode_long(self._encoder, self._extractor, clip)[None].to(
                    weights.device, weights.dtype
                )
            )[0]
            for clip in request.clips
        ]
        text = prompt_text(self._tokenizer.render_chat, request, [a.shape[0] for a in audio])
        ids = torch.tensor([self._tokenizer.text_ids(text)])
        embeds = (
            self._glimmer.embed_with_audio(ids, audio) if audio else self._glimmer.embed_text(ids)
        )
        return embeds, ids.shape[1]
