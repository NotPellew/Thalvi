"""Runs one chat request through the audio encoder, projector and Glimmer, one at a time.

The engine owns the loaded models and a lock, so requests queue and generate in turn.
`Engine.run` is blocking and reports through `emit`: `Ready` once the first token is
chosen, then a `Step` per token with the events it completed, then `Finished`.
Setting `cancelled` stops it before the next token. Every request ends with one
`[engine]` log line of its timings (`StepTimer`), the serving log's view of speed.

A spoken turn without tools is answered in echo mode (see `thalvi_train.echo`): the
prompt ends in the echo prefill, and once the echo is over the engine sends the
handoff tokens to the decoder, which feeds them into the running KV cache. The reply
parser is given the same text, so it switches from reasoning to the answer exactly
where the model would have.
"""

import threading
import time
from collections.abc import Callable, Generator
from dataclasses import dataclass
from typing import Literal

import torch
from transformers import WhisperFeatureExtractor
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.chat import ChatRequest, prompt_text
from thalvi_train.decode import decode_steps, sampler
from thalvi_train.echo import HANDOFF, PREFILL, EchoWatch, ReplyMode, uses_echo
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


class StepTimer:
    """Times a request's prefill and each decoding step after it, for the serving log."""

    def __init__(self, clock: Callable[[], float] = time.perf_counter) -> None:
        self._clock = clock
        self._start = clock()
        self._marks: list[float] = []

    def mark(self) -> None:
        """Notes that a step, the first being the prefill's, has just come out."""
        self._marks.append(self._clock())

    def summary(self, prompt_tokens: int) -> str:
        """One line: prefill time, decoding rate, and the slowest step and its index."""
        if not self._marks:
            return f"[engine] prompt {prompt_tokens} tokens, cancelled before the first token"
        gaps = [b - a for a, b in zip(self._marks, self._marks[1:], strict=False)]
        line = (
            f"[engine] prompt {prompt_tokens} tokens, prefill {self._marks[0] - self._start:.2f} s"
        )
        if gaps:
            slowest = max(range(len(gaps)), key=gaps.__getitem__)
            line += (
                f", {len(gaps)} steps at {len(gaps) / sum(gaps):.1f}/s,"
                f" slowest {gaps[slowest]:.2f} s at step {slowest + 1}"
            )
        return line


def _resume(
    steps: Generator[list[int | None], list[int] | None, None], forced: list[int] | None
) -> list[int | None] | None:
    """The next step, after feeding `forced` if given; `None` once the generator is over."""
    try:
        return steps.send(forced)
    except StopIteration:
        return None


class Engine:
    """Glimmer with its hearing: encoder, projector, decoder and tokenizer."""

    def __init__(
        self,
        glimmer: FrozenGlimmer,
        tokenizer: ChatTokenizer,
        projector: AudioProjector,
        encoder: WhisperEncoder,
        extractor: WhisperFeatureExtractor,
        reply_mode: ReplyMode = "echo",
    ) -> None:
        self._glimmer = glimmer
        self._tokenizer = tokenizer
        self._projector = projector
        self._encoder = encoder
        self._extractor = extractor
        self._reply_mode = reply_mode
        self._lock = threading.Lock()

    def run(
        self, request: ChatRequest, emit: Callable[[Update], None], cancelled: threading.Event
    ) -> None:
        """Generates the reply to `request`, waiting for any earlier request to finish first.

        Returns without a `Finished` if `cancelled` is set. Errors propagate; if they
        come before `Ready`, nothing was emitted.
        """
        with self._lock, torch.no_grad():
            if cancelled.is_set():
                return
            echoing = uses_echo(request, self._reply_mode)
            embeds, prompt_tokens = self._prompt(request, echoing=echoing)
            parser = ReplyParser(tool_parameter_types(request.tools))
            decoder = IncrementalDecoder(self._tokenizer.decode)
            watch = EchoWatch() if echoing else None
            handoff = self._tokenizer.text_ids(HANDOFF)
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
            timer = StepTimer()
            try:
                step = _resume(steps, None)
                timer.mark()
                while step is not None:
                    if cancelled.is_set():
                        return
                    events: list[Event] = []
                    if generated == 0:
                        emit(Ready())
                        if watch:
                            events = parser.feed(PREFILL)
                    generated += 1
                    text, forced = decoder.push(step[0]), None
                    if watch:
                        kept, ended = watch.push(text)
                        if ended:
                            head = [step[0]] if kept == text else self._tokenizer.text_ids(kept)
                            forced = head + handoff
                        text = kept
                    events += parser.feed(text)
                    called = called or any(isinstance(e, ToolCall) for e in events)
                    emit(Step(events))
                    if parser.done:
                        break
                    if forced is not None:
                        parser.feed(HANDOFF)
                        prompt_tokens += len(handoff)
                        watch, decoder = None, IncrementalDecoder(self._tokenizer.decode)
                    step = _resume(steps, forced)
                    timer.mark()
            finally:
                steps.close()
                print(timer.summary(prompt_tokens), flush=True)
            events = parser.feed(decoder.flush()) + parser.finish()
            if events:
                called = called or any(isinstance(e, ToolCall) for e in events)
                emit(Step(events))
            reason: FinishReason = "tool_calls" if called else "stop"
            if not parser.done:
                reason = "length"
            emit(Finished(reason, prompt_tokens, generated))

    def _encoder_autocast(self) -> torch.autocast:
        """bf16 autocast for the encoder and projector, which hold fp16 and bf16 weights.

        The decoder's weights are bf16 already, so decoding runs outside it.
        """
        kind = self._glimmer.device.type
        return torch.autocast(kind, dtype=torch.bfloat16, enabled=kind == "cuda")

    def _prompt(self, request: ChatRequest, *, echoing: bool) -> tuple[torch.Tensor, int]:
        """The prompt's embeddings `(1, length, hidden)`, clips spliced in, and its length.

        The echo prefill is tokenized apart from the rendered text, the way training
        tokenizes a response apart from its prompt.
        """
        weights = next(self._projector.parameters())
        with self._encoder_autocast():
            audio = [
                self._projector(
                    encode_long(self._encoder, self._extractor, clip)[None].to(
                        weights.device, weights.dtype
                    )
                )[0]
                for clip in request.clips
            ]
        text = prompt_text(self._tokenizer.render_chat, request, [a.shape[0] for a in audio])
        ids = self._tokenizer.text_ids(text) + (
            self._tokenizer.text_ids(PREFILL) if echoing else []
        )
        ids = torch.tensor([ids])
        embeds = (
            self._glimmer.embed_with_audio(ids, audio) if audio else self._glimmer.embed_text(ids)
        )
        return embeds, ids.shape[1]
