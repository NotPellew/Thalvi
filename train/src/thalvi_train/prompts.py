"""Prompt rendering for distillation: the same question as text (teacher) and as audio (student)."""

from dataclasses import dataclass
from typing import Any, Protocol

from transformers import PreTrainedTokenizerBase

from thalvi_train.records import Record, Task

Turn = tuple[str, str]
History = tuple[Turn, ...]

SYSTEM_PROMPT = "You are Thalvi, a voice assistant. Reply in short spoken sentences, no markdown."
CONTINUE_PREFIX = "Continue the following text: "
AUDIO_TOKEN_TEXT = "<|reserved_special_token_1000|>"
REASONING_STRENGTH = "low"
GENERATION_PROMPT = "<|start|>assistant"


class PromptTokenizer(Protocol):
    """What prompts and generations need from a tokenizer; the real one wraps Glimmer's."""

    def chat_ids(self, system: str, user: str, history: History = ()) -> list[int]:
        """Ids of the system turn, the `history` of `(user, assistant)` text turns, and the
        user turn, ending after the user's `<|eot|>`.
        """
        ...

    def text_ids(self, text: str) -> list[int]:
        """Ids of `text` with special tokens parsed and none added."""
        ...

    def decode(self, ids: list[int]) -> str:
        """The text of `ids`, special tokens written out."""
        ...


class ChatTokenizer(PromptTokenizer, Protocol):
    """A `PromptTokenizer` that also renders whole conversations, as serving needs."""

    def render_chat(self, messages: list[dict[str, Any]], tools: list[dict[str, Any]]) -> str:
        """The prompt text of `messages` and `tools`, ending at the generation prompt."""
        ...


class GlimmerTokenizer:
    """`PromptTokenizer` over Glimmer's Hugging Face tokenizer and chat template."""

    def __init__(self, tokenizer: PreTrainedTokenizerBase) -> None:
        self.tokenizer = tokenizer

    def chat_ids(self, system: str, user: str, history: History = ()) -> list[int]:
        messages = [{"role": "system", "content": system}]
        for earlier_user, earlier_assistant in history:
            messages.append({"role": "user", "content": earlier_user})
            messages.append({"role": "assistant", "content": earlier_assistant})
        messages.append({"role": "user", "content": user})
        rendered = self.tokenizer.apply_chat_template(
            messages,
            tokenize=False,
            add_generation_prompt=False,
            reasoning_strength=REASONING_STRENGTH,
        )
        return self.text_ids(rendered)

    def render_chat(self, messages: list[dict[str, Any]], tools: list[dict[str, Any]]) -> str:
        return self.tokenizer.apply_chat_template(
            messages,
            tools=tools or None,
            tokenize=False,
            add_generation_prompt=True,
            reasoning_strength=REASONING_STRENGTH,
        )

    def text_ids(self, text: str) -> list[int]:
        return self.tokenizer.encode(text, add_special_tokens=False)

    def decode(self, ids: list[int]) -> str:
        return self.tokenizer.decode(ids, skip_special_tokens=False)


@dataclass(frozen=True)
class Example:
    """Teacher and student token ids for one record, sharing `response_ids` as their tail."""

    teacher_ids: list[int]
    student_ids: list[int]
    response_ids: list[int]


def user_turn(task: Task, body: str) -> str:
    """The user turn for `task`, with `body` as the request or the text to continue."""
    return body if task == "answer" else CONTINUE_PREFIX + body


def user_message(record: Record, audio: str | None = None) -> str:
    """The user turn: the transcript, or `audio` standing in for it."""
    return user_turn(record.task, record.text if audio is None else audio)


def generation_ids(
    task: Task, text: str, tokenizer: PromptTokenizer, history: History = ()
) -> list[int]:
    """Ids the teacher is prompted with for a transcript: the chat turns plus `GENERATION_PROMPT`.

    Every `Record.response` is `GENERATION_PROMPT` plus what the teacher generated
    from these ids with no `history`, so `build_example` trains on the prompt the
    teacher saw, plus any earlier turns.
    """
    return tokenizer.chat_ids(SYSTEM_PROMPT, user_turn(task, text), history) + tokenizer.text_ids(
        GENERATION_PROMPT
    )


def build_example(
    record: Record,
    tokenizer: PromptTokenizer,
    audio_tokens: int,
    max_response_tokens: int | None = None,
    history: History = (),
) -> Example:
    """Renders `record` for the teacher (text) and the student (`audio_tokens` placeholders).

    Both prompts open with the same `history` of earlier text turns and differ only in
    the current user turn; the response is the one stored with `record`, whatever the
    history, because the echo of the current request that carries the audio signal
    does not depend on it.

    With `max_response_tokens`, only the response's first tokens are kept: under
    teacher forcing, tokens after Glimmer's echo of the request read the transcript
    from context and carry little signal about the audio.
    """
    response_ids = tokenizer.text_ids(record.response)[:max_response_tokens]
    placeholders = AUDIO_TOKEN_TEXT * audio_tokens
    return Example(
        teacher_ids=tokenizer.chat_ids(SYSTEM_PROMPT, user_message(record), history) + response_ids,
        student_ids=tokenizer.chat_ids(SYSTEM_PROMPT, user_message(record, placeholders), history)
        + response_ids,
        response_ids=response_ids,
    )
