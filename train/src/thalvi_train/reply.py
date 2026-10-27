"""Reads Glimmer's generated text: incremental token decoding and the channel format.

A generation is a run of messages, each `<|start|>assistant to=RECIPIENT<|message|>BODY`
ended by `<|eom|>` (more follows) or `<|eot|>` (the turn is over). The prompt already
ends in `<|start|>assistant`, so the text starts at the header's ` to=...`. `to=self`
is the reasoning, `to=user` the answer, and any other recipient a tool call whose
body is an ATEM block:

    <atem:function_calls>
    <atem:invoke name="get_weather">
    <atem:parameter name="city">Paris</atem:parameter>
    </atem:invoke>
    </atem:function_calls>

Strings are written raw and other values as JSON, so a parameter is a string if its
declared type says so and otherwise JSON when it parses. Special-token text never
reaches a delta, and a tool call is reported only once its message is complete.
"""

import json
import re
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any

_MARKER = re.compile(r"<\|[a-z_0-9]*\|>")
_UNFINISHED_MARKER = re.compile(r"<(\|[a-z_0-9]*\|?)?$")
_RECIPIENT = re.compile(r"\bto=(\S+)")
_INVOKE = re.compile(r'<atem:invoke name="(.*?)">(.*?)</atem:invoke>', re.DOTALL)
_PARAMETER = re.compile(r'<atem:parameter name="(.*?)">(.*?)</atem:parameter>', re.DOTALL)

START = "<|start|>"
MESSAGE = "<|message|>"
EOM = "<|eom|>"
EOT = "<|eot|>"
REPLACEMENT = "�"


@dataclass(frozen=True)
class Reasoning:
    """A piece of Glimmer's reasoning."""

    text: str


@dataclass(frozen=True)
class Content:
    """A piece of the answer for the user."""

    text: str


@dataclass(frozen=True)
class ToolCall:
    """A complete call to `name`, its `arguments` already typed."""

    name: str
    arguments: dict[str, Any]

    def arguments_json(self) -> str:
        """The arguments as the JSON string OpenAI's `function.arguments` carries."""
        return json.dumps(self.arguments, ensure_ascii=False)


Event = Reasoning | Content | ToolCall


class IncrementalDecoder:
    """Turns a stream of token ids into text without splitting a character.

    A token may end in the middle of a multi-byte character; its text then ends in
    U+FFFD and is held back until a later token completes it.
    """

    def __init__(self, decode: Callable[[list[int]], str]) -> None:
        self._decode = decode
        self._ids: list[int] = []
        self._prefix = 0
        self._read = 0

    def push(self, token_id: int) -> str:
        """The text `token_id` completes, which may be empty."""
        self._ids.append(token_id)
        text = self._decode(self._ids[self._prefix :])
        if text.endswith(REPLACEMENT):
            return ""
        seen = self._decode(self._ids[self._prefix : self._read])
        self._prefix, self._read = self._read, len(self._ids)
        return text[len(seen) :]

    def flush(self) -> str:
        """Whatever is still held back, undecodable bytes included."""
        text = self._decode(self._ids[self._prefix :])
        seen = self._decode(self._ids[self._prefix : self._read])
        self._prefix = self._read = len(self._ids)
        return text[len(seen) :]


def tool_parameter_types(tools: Sequence[Mapping[str, Any]]) -> dict[str, dict[str, Any]]:
    """Each OpenAI function tool's name mapped to its parameters' JSON schemas."""
    return {
        tool["function"]["name"]: tool["function"].get("parameters", {}).get("properties", {})
        for tool in tools
    }


def _declares_string(schema: Mapping[str, Any]) -> bool:
    declared = schema.get("type")
    kinds = declared if isinstance(declared, list) else [declared]
    return "string" in kinds and set(kinds) <= {"string", "null"}


def _typed(raw: str, schema: Mapping[str, Any] | None) -> Any:
    if schema is not None and _declares_string(schema):
        return raw
    try:
        return json.loads(raw)
    except ValueError:
        return raw


def parse_tool_calls(body: str, parameter_types: Mapping[str, Mapping[str, Any]]) -> list[ToolCall]:
    """The calls in an ATEM `body`; text outside well-formed invokes is ignored.

    `parameter_types` maps tool names to their parameters' schemas, as
    `tool_parameter_types` gives them.
    """
    calls = []
    for name, invoke in _INVOKE.findall(body):
        schemas = parameter_types.get(name, {})
        arguments = {key: _typed(raw, schemas.get(key)) for key, raw in _PARAMETER.findall(invoke)}
        calls.append(ToolCall(name, arguments))
    return calls


class ReplyParser:
    """Splits generated text, fed in pieces of any size, into reasoning, answer and tool calls.

    Text between a message's end and the next `<|start|>` is ignored, and so is
    everything after `<|eot|>`; `done` says it was seen.
    """

    def __init__(self, parameter_types: Mapping[str, Mapping[str, Any]] | None = None) -> None:
        self._parameter_types = parameter_types or {}
        self._pending = ""
        self._header: str | None = ""
        self._recipient = ""
        self._body = ""
        self.done = False

    def feed(self, text: str) -> list[Event]:
        """The events `text` completes; text that may start a special token waits for more."""
        events: list[Event] = []
        self._pending += text
        while self._pending and not self.done:
            marker = _MARKER.search(self._pending)
            if marker:
                self._text(self._pending[: marker.start()], events)
                self._pending = self._pending[marker.end() :]
                self._marker(marker.group(), events)
                continue
            unfinished = _UNFINISHED_MARKER.search(self._pending)
            cut = unfinished.start() if unfinished else len(self._pending)
            self._text(self._pending[:cut], events)
            self._pending = self._pending[cut:]
            break
        return events

    def finish(self) -> list[Event]:
        """The events left at the end of generation; an unfinished tool call is dropped."""
        events: list[Event] = []
        if not self._pending.startswith("<|"):
            self._text(self._pending, events)
        self._pending = ""
        return events

    def _text(self, text: str, events: list[Event]) -> None:
        if not text:
            return
        if self._header is not None:
            self._header += text
        elif self._recipient == "self":
            events.append(Reasoning(text))
        elif self._recipient == "user":
            events.append(Content(text))
        else:
            self._body += text

    def _marker(self, marker: str, events: list[Event]) -> None:
        if marker == START:
            self._header = ""
            self._body = ""
        elif marker == MESSAGE and self._header is not None:
            found = _RECIPIENT.search(self._header)
            self._recipient = found.group(1) if found else "user"
            self._header = None
        elif marker in (EOM, EOT) and self._header is None:
            if self._recipient not in ("self", "user"):
                events.extend(parse_tool_calls(self._body, self._parameter_types))
            self._header = ""
            self._body = ""
        self.done = self.done or marker == EOT
