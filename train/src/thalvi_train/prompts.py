"""Prompt rendering for distillation: the same question as text (teacher) and as audio (student)."""

from dataclasses import dataclass
from typing import Protocol

from transformers import PreTrainedTokenizerBase

from thalvi_train.records import Record

SYSTEM_PROMPT = "You are Thalvi, a voice assistant. Reply in short spoken sentences, no markdown."
CONTINUE_PREFIX = "Continue the following text: "
AUDIO_TOKEN_TEXT = "<|reserved_special_token_1000|>"
REASONING_STRENGTH = "low"


class PromptTokenizer(Protocol):
    """What prompt building needs from a tokenizer; the real one wraps Glimmer's."""

    def chat_ids(self, system: str, user: str) -> list[int]:
        """Ids of the system and user turns, ending after the user's `<|eot|>`."""
        ...

    def text_ids(self, text: str) -> list[int]:
        """Ids of `text` with special tokens parsed and none added."""
        ...


class GlimmerTokenizer:
    """`PromptTokenizer` over Glimmer's Hugging Face tokenizer and chat template."""

    def __init__(self, tokenizer: PreTrainedTokenizerBase) -> None:
        self.tokenizer = tokenizer

    def chat_ids(self, system: str, user: str) -> list[int]:
        rendered = self.tokenizer.apply_chat_template(
            [{"role": "system", "content": system}, {"role": "user", "content": user}],
            tokenize=False,
            add_generation_prompt=False,
            reasoning_strength=REASONING_STRENGTH,
        )
        return self.text_ids(rendered)

    def text_ids(self, text: str) -> list[int]:
        return self.tokenizer.encode(text, add_special_tokens=False)


@dataclass(frozen=True)
class Example:
    """Teacher and student token ids for one record, sharing `response_ids` as their tail."""

    teacher_ids: list[int]
    student_ids: list[int]
    response_ids: list[int]


def user_message(record: Record, audio: str | None = None) -> str:
    """The user turn: the transcript, or `audio` standing in for it."""
    body = record.text if audio is None else audio
    return body if record.task == "answer" else CONTINUE_PREFIX + body


def build_example(record: Record, tokenizer: PromptTokenizer, audio_tokens: int) -> Example:
    """Renders `record` for the teacher (text) and the student (`audio_tokens` placeholders)."""
    response_ids = tokenizer.text_ids(record.response)
    placeholders = AUDIO_TOKEN_TEXT * audio_tokens
    return Example(
        teacher_ids=tokenizer.chat_ids(SYSTEM_PROMPT, user_message(record)) + response_ids,
        student_ids=tokenizer.chat_ids(SYSTEM_PROMPT, user_message(record, placeholders))
        + response_ids,
        response_ids=response_ids,
    )
