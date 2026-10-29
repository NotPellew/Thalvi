"""The echo reply mode: a spoken turn is answered after a one-line echo instead of full reasoning.

Glimmer reasons for 100 to 200 tokens before it answers, which is seconds before the
first answer token. The adapter was trained to open its reasoning with the request it
heard, so the engine starts that reasoning for it, `PREFILL`, lets the model finish the
line with what it heard, and ends the message itself with `HANDOFF`. The answer then
follows at once. Prefilling an empty reasoning instead makes the model repeat the
request as its answer in about two cases of five.

This module owns when the mode applies and where an echo ends; the engine drives the
decoding and feeds `HANDOFF` into the running KV cache.
"""

from typing import Literal

from thalvi_train.chat import ChatRequest

ReplyMode = Literal["echo", "reasoning"]

OPENING = "The user said:"
PREFILL = " to=self<|message|>" + OPENING
HANDOFF = "<|eom|><|start|>assistant to=user<|message|>"
MAX_ECHO_TOKENS = 40
_END_MARKERS = ("<|eom|>", "<|eot|>")
_CLOSING_QUOTES = {'"': '"', "“": "”"}


def uses_echo(request: ChatRequest, mode: ReplyMode) -> bool:
    """Whether `request` is answered in echo mode: the latest user message holds audio.

    A request with tools needs real reasoning to choose them, so it never echoes.
    """
    if mode != "echo" or request.tools:
        return False
    latest = next((m for m in reversed(request.messages) if m["role"] == "user"), None)
    content = latest["content"] if latest else ""
    return isinstance(content, list) and any(part["type"] == "input_audio" for part in content)


def echo_end(text: str) -> int | None:
    """Where the echo ends in `text`, the echo's decoded text so far; `None` while it goes on.

    It ends at the earliest of the first line break once there is text, the quote
    closing a quoted request (the text then opens with `"` or `“`), and an `<|eom|>`
    or `<|eot|>` the model wrote itself. The result is the length of the echo,
    closing quote included, line break and marker excluded.
    """
    ends = [text.find(marker) for marker in _END_MARKERS]
    body = len(text) - len(text.lstrip())
    if body < len(text):
        ends.append(text.find("\n", body))
        closing = _CLOSING_QUOTES.get(text[body])
        if closing:
            at = text.find(closing, body + 1)
            ends.append(at + 1 if at >= 0 else -1)
    found = [end for end in ends if end >= 0]
    return min(found) if found else None


def heard(reasoning: str) -> str:
    """The request an echo heard: `reasoning` after `OPENING`, up to `echo_end`, quotes dropped."""
    text = reasoning.removeprefix(OPENING)
    end = echo_end(text)
    line = (text if end is None else text[:end]).strip()
    for opening, closing in _CLOSING_QUOTES.items():
        if line.startswith(opening):
            line = line[len(opening) :].removesuffix(closing)
    return line.strip()


class EchoWatch:
    """Follows the echo piece by piece and says where it ends.

    The echo also ends after `MAX_ECHO_TOKENS` pieces, since Glimmer may go on
    reasoning on the same line.
    """

    def __init__(self) -> None:
        self._text = ""
        self._pieces = 0

    def push(self, piece: str) -> tuple[str, bool]:
        """The part of the next decoded `piece` that is echo, and whether the echo ended in it."""
        self._pieces += 1
        self._text += piece
        end = echo_end(self._text)
        if end is None:
            return piece, self._pieces >= MAX_ECHO_TOKENS
        return piece[: max(0, len(piece) - (len(self._text) - end))], True
