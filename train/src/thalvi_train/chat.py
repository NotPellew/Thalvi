"""Chat Completions requests: validation at the boundary, and the Glimmer prompt text they become.

`parse_request` accepts OpenAI's shape, ignores fields it has no use for
(`reasoning_effort`, `model`, `tool_choice`, ...), and turns anything else wrong
into a `RequestError` that renders as OpenAI's 400 body. Audio parts are decoded to
16 kHz clips there. The chat template only renders `text`, `image` and `video`
parts and silently drops the rest, so `prompt_text` hands it text parts alone and
checks that every audio placeholder survived.
"""

import base64
import binascii
import io
import json
import re
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any

import numpy as np

from thalvi_train.features import SAMPLE_RATE, FeatureError, decode_audio
from thalvi_train.prompts import AUDIO_TOKEN_TEXT

MAX_AUDIO_SECONDS = 300
DEFAULT_MAX_TOKENS = 1024
_TOOL_NAME = re.compile(r"[A-Za-z0-9_.\-]{1,64}")
_DEFAULT_PARAMETERS = {"type": "object", "properties": {}}
_ROLES = {"system", "developer", "user", "assistant", "tool"}

Message = dict[str, Any]
Render = Callable[[list[Message], list[dict[str, Any]]], str]


class RequestError(ValueError):
    """A request that cannot be served, as OpenAI reports it: HTTP 400, `invalid_request_error`."""

    def __init__(self, message: str, param: str | None = None) -> None:
        super().__init__(message)
        self.param = param

    def body(self) -> dict[str, Any]:
        """The JSON error body."""
        return {
            "error": {
                "message": str(self),
                "type": "invalid_request_error",
                "param": self.param,
                "code": None,
            }
        }


class PromptError(RuntimeError):
    """The chat template did not render what was handed to it."""


@dataclass(frozen=True)
class ChatRequest:
    """A validated request.

    `messages` are ready for the chat template except that an audio part is
    `{"type": "input_audio", "clip": i}`, standing for `clips[i]`.
    """

    messages: list[Message]
    clips: list[np.ndarray]
    tools: list[dict[str, Any]]
    max_tokens: int
    temperature: float
    top_p: float
    include_usage: bool


def _fail(param: str, message: str) -> RequestError:
    return RequestError(f"{param}: {message}", param)


def _text(value: Any, param: str) -> str:
    if not isinstance(value, str):
        raise _fail(param, "must be a string")
    if AUDIO_TOKEN_TEXT in value:
        raise _fail(param, "contains the reserved audio placeholder")
    return value


def _audio(part: Any, param: str, clips: list[np.ndarray]) -> Message:
    audio = part.get("input_audio")
    if not isinstance(audio, dict):
        raise _fail(f"{param}.input_audio", "must be an object")
    if audio.get("format") != "wav":
        raise _fail(f"{param}.input_audio.format", "only 'wav' is supported")
    data = audio.get("data")
    if not isinstance(data, str):
        raise _fail(f"{param}.input_audio.data", "must be a base64 string")
    try:
        raw = base64.b64decode(data, validate=True)
    except (binascii.Error, ValueError) as error:
        raise _fail(f"{param}.input_audio.data", "is not valid base64") from error
    try:
        clip = decode_audio(io.BytesIO(raw))
    except FeatureError as error:
        raise _fail(f"{param}.input_audio.data", str(error)) from error
    clips.append(clip)
    if sum(len(c) for c in clips) > MAX_AUDIO_SECONDS * SAMPLE_RATE:
        raise _fail(param, f"audio longer than {MAX_AUDIO_SECONDS} s in total")
    return {"type": "input_audio", "clip": len(clips) - 1}


def _parts(content: Any, param: str, clips: list[np.ndarray], *, audio: bool) -> str | list:
    if isinstance(content, str):
        return _text(content, param)
    if not isinstance(content, list) or not content:
        raise _fail(param, "must be a string or a non-empty list of content parts")
    parts: list[Message] = []
    for i, part in enumerate(content):
        where = f"{param}[{i}]"
        kind = part.get("type") if isinstance(part, dict) else None
        if kind == "text":
            parts.append({"type": "text", "text": _text(part.get("text"), f"{where}.text")})
        elif kind == "input_audio" and audio:
            parts.append(_audio(part, where, clips))
        else:
            raise _fail(f"{where}.type", f"unsupported content part type {kind!r} for this role")
    return parts


def _flat(content: Any, param: str) -> str:
    parts = _parts(content, param, [], audio=False)
    return parts if isinstance(parts, str) else "".join(p["text"] for p in parts)


def _tool_call(call: Any, param: str) -> Message:
    function = call.get("function") if isinstance(call, dict) else None
    if not isinstance(function, dict) or not isinstance(call.get("id"), str):
        raise _fail(param, "must have a string id and a function object")
    name, arguments = function.get("name"), function.get("arguments", "{}")
    if not isinstance(name, str) or not _TOOL_NAME.fullmatch(name):
        raise _fail(f"{param}.function.name", "is not a valid function name")
    if isinstance(arguments, str):
        try:
            arguments = _json_object(arguments)
        except ValueError as error:
            raise _fail(f"{param}.function.arguments", "must be a JSON object string") from error
    if not isinstance(arguments, dict):
        raise _fail(f"{param}.function.arguments", "must be a JSON object string")
    return {
        "id": call["id"],
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }


def _json_object(text: str) -> dict:
    value = json.loads(text or "{}")
    if not isinstance(value, dict):
        raise ValueError("not an object")
    return value


def _assistant(raw: Mapping[str, Any], param: str) -> Message:
    message: Message = {"role": "assistant", "content": ""}
    if raw.get("content") is not None:
        message["content"] = _flat(raw["content"], f"{param}.content")
    if raw.get("reasoning_content"):
        message["reasoning_content"] = _text(raw["reasoning_content"], f"{param}.reasoning_content")
    calls = raw.get("tool_calls")
    if calls:
        if not isinstance(calls, list):
            raise _fail(f"{param}.tool_calls", "must be a list")
        message["tool_calls"] = [
            _tool_call(call, f"{param}.tool_calls[{i}]") for i, call in enumerate(calls)
        ]
    return message


def _tool_result(raw: Mapping[str, Any], param: str, known_calls: set[str]) -> Message:
    message: Message = {"role": "tool", "content": _flat(raw.get("content"), f"{param}.content")}
    if raw.get("name"):
        message["name"] = _text(raw["name"], f"{param}.name")
    call_id = raw.get("tool_call_id")
    if call_id is not None:
        message["tool_call_id"] = _text(call_id, f"{param}.tool_call_id")
    if "name" not in message and call_id not in known_calls:
        raise _fail(f"{param}.tool_call_id", "matches no earlier assistant tool call")
    return message


def _messages(raw: Any, clips: list[np.ndarray]) -> list[Message]:
    if not isinstance(raw, list) or not raw:
        raise _fail("messages", "must be a non-empty list")
    messages: list[Message] = []
    known_calls: set[str] = set()
    for i, item in enumerate(raw):
        param = f"messages[{i}]"
        role = item.get("role") if isinstance(item, dict) else None
        if role not in _ROLES:
            raise _fail(f"{param}.role", f"must be one of {sorted(_ROLES)}")
        if role == "assistant":
            message = _assistant(item, param)
            known_calls |= {call["id"] for call in message.get("tool_calls", [])}
        elif role == "tool":
            message = _tool_result(item, param, known_calls)
        elif role == "user":
            message = {
                "role": role,
                "content": _parts(item.get("content"), param, clips, audio=True),
            }
        else:
            message = {"role": "system", "content": _flat(item.get("content"), f"{param}.content")}
        messages.append(message)
    if messages[-1]["role"] not in ("user", "tool"):
        raise _fail("messages", "the last message must be from the user or a tool")
    return messages


def _tools(raw: Any) -> list[dict[str, Any]]:
    if raw is None:
        return []
    if not isinstance(raw, list):
        raise _fail("tools", "must be a list")
    tools = []
    for i, tool in enumerate(raw):
        param = f"tools[{i}]"
        function = tool.get("function") if isinstance(tool, dict) else None
        if not isinstance(function, dict) or tool.get("type") != "function":
            raise _fail(param, "must be a function tool")
        name = function.get("name")
        if not isinstance(name, str) or not _TOOL_NAME.fullmatch(name):
            raise _fail(f"{param}.function.name", "is not a valid function name")
        parameters = function.get("parameters", _DEFAULT_PARAMETERS)
        description = function.get("description", "")
        if not isinstance(parameters, dict) or not isinstance(description, str):
            raise _fail(param, "needs a string description and an object parameters schema")
        tools.append(
            {
                "type": "function",
                "function": {"name": name, "description": description, "parameters": parameters},
            }
        )
    return tools


def _number(body: Mapping[str, Any], key: str, default: float, low: float, high: float) -> float:
    value = body.get(key)
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, int | float) or not low <= value <= high:
        raise _fail(key, f"must be a number from {low:g} to {high:g}")
    return float(value)


def _max_tokens(body: Mapping[str, Any]) -> int:
    for key in ("max_completion_tokens", "max_tokens"):
        value = body.get(key)
        if value is not None:
            if isinstance(value, bool) or not isinstance(value, int) or value < 1:
                raise _fail(key, "must be a positive integer")
            return value
    return DEFAULT_MAX_TOKENS


def parse_request(body: Any) -> ChatRequest:
    """Validates a decoded JSON request body; raises `RequestError` naming the bad field."""
    if not isinstance(body, dict):
        raise RequestError("the body must be a JSON object")
    if body.get("stream") is not True:
        raise _fail("stream", "only streaming is supported; set it to true")
    options = body.get("stream_options") or {}
    if not isinstance(options, dict):
        raise _fail("stream_options", "must be an object")
    clips: list[np.ndarray] = []
    return ChatRequest(
        messages=_messages(body.get("messages"), clips),
        clips=clips,
        tools=_tools(body.get("tools")),
        max_tokens=_max_tokens(body),
        temperature=_number(body, "temperature", 0.0, 0.0, 2.0),
        top_p=_number(body, "top_p", 1.0, 1e-6, 1.0),
        include_usage=options.get("include_usage") is True,
    )


def _with_placeholders(messages: Sequence[Message], token_counts: Sequence[int]) -> list[Message]:
    placed = []
    for message in messages:
        content = message["content"]
        if isinstance(content, list):
            content = [
                {"type": "text", "text": AUDIO_TOKEN_TEXT * token_counts[part["clip"]]}
                if part["type"] == "input_audio"
                else part
                for part in content
            ]
        placed.append({**message, "content": content})
    return placed


def prompt_text(
    render: Render,
    request: ChatRequest,
    token_counts: Sequence[int],
) -> str:
    """The prompt: `request` through the chat template, `token_counts[i]` placeholders per clip.

    Raises `PromptError` if the template got anything but text parts or lost a placeholder.
    """
    messages = _with_placeholders(request.messages, token_counts)
    for message in messages:
        if isinstance(message["content"], list) and any(
            part["type"] != "text" for part in message["content"]
        ):
            raise PromptError("only text parts may reach the chat template")
    text = render(messages, request.tools)
    if text.count(AUDIO_TOKEN_TEXT) != sum(token_counts):
        raise PromptError("the chat template changed the number of audio placeholders")
    return text
