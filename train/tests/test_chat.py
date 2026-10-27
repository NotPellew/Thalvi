import copy
from pathlib import Path

import pytest
from transformers import AutoTokenizer
from wav import audio_part

from thalvi_train.chat import (
    DEFAULT_MAX_TOKENS,
    ChatRequest,
    PromptError,
    RequestError,
    parse_request,
    prompt_text,
)
from thalvi_train.glimmer import AUDIO_TOKEN_ID
from thalvi_train.prompts import AUDIO_TOKEN_TEXT, GlimmerTokenizer

GLIMMER_TOKENIZER = Path.home() / "models/Muse-Glimmer-30B-text"
WEATHER = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Weather in a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
    },
}
CALL = {
    "id": "call_1",
    "type": "function",
    "function": {"name": "get_weather", "arguments": '{"city": "Paris", "days": 3, "ok": true}'},
}


def body(*messages: dict, **extra) -> dict:
    return {"stream": True, "messages": list(messages), **extra}


def user(content) -> dict:
    return {"role": "user", "content": content}


@pytest.fixture(scope="module")
def glimmer_tokenizer() -> GlimmerTokenizer:
    if not (GLIMMER_TOKENIZER / "tokenizer.json").exists():
        pytest.skip("the Glimmer tokenizer is not on this machine")
    return GlimmerTokenizer(AutoTokenizer.from_pretrained(GLIMMER_TOKENIZER))


def test_a_plain_request_gets_the_defaults_and_ignores_fields_it_does_not_use() -> None:
    request = parse_request(
        body(
            {"role": "system", "content": "Be brief."},
            user("hi"),
            reasoning_effort="minimal",
            model="anything",
            tool_choice="auto",
            n=1,
            presence_penalty=0.5,
        )
    )
    assert request.messages == [
        {"role": "system", "content": "Be brief."},
        {"role": "user", "content": "hi"},
    ]
    assert (request.max_tokens, request.temperature, request.top_p) == (
        DEFAULT_MAX_TOKENS,
        0.0,
        1.0,
    )
    assert request.tools == []
    assert request.clips == []
    assert not request.include_usage


def test_sampling_limits_and_usage_options_are_read() -> None:
    request = parse_request(
        body(
            user("hi"),
            max_tokens=10,
            max_completion_tokens=20,
            temperature=0.7,
            top_p=0.9,
            stream_options={"include_usage": True},
        )
    )
    assert (request.max_tokens, request.temperature, request.top_p) == (20, 0.7, 0.9)
    assert request.include_usage
    assert parse_request(body(user("hi"), max_tokens=10)).max_tokens == 10


def test_audio_is_decoded_to_16khz_mono_clips_in_order() -> None:
    request = parse_request(
        body(
            user(
                [
                    audio_part(samples=24_000, rate=24_000, channels=2),
                    {"type": "text", "text": "and"},
                    audio_part(samples=8_000),
                ]
            )
        )
    )
    assert [len(clip) for clip in request.clips] == [16_000, 8_000]
    assert request.messages[0]["content"] == [
        {"type": "input_audio", "clip": 0},
        {"type": "text", "text": "and"},
        {"type": "input_audio", "clip": 1},
    ]


def _wav(**kwargs):
    return audio_part(**kwargs)["input_audio"]["data"]


BAD_REQUESTS = {
    "not an object": ([], None),
    "not streaming": ({"messages": [user("hi")]}, "stream"),
    "stream false": ({"stream": False, "messages": [user("hi")]}, "stream"),
    "no messages": (body(), "messages"),
    "unknown role": (body({"role": "robot", "content": "x"}), "messages[0].role"),
    "last is assistant": (body(user("hi"), {"role": "assistant", "content": "x"}), "messages"),
    "unknown part type": (
        body(user([{"type": "image_url", "image_url": {}}])),
        "messages[0][0].type",
    ),
    "empty parts": (body(user([])), "messages[0]"),
    "audio in a system message": (
        body({"role": "system", "content": [audio_part()]}, user("hi")),
        "messages[0].content[0].type",
    ),
    "audio format": (
        body(user([{"type": "input_audio", "input_audio": {"data": _wav(), "format": "mp3"}}])),
        "messages[0][0].input_audio.format",
    ),
    "audio without data": (
        body(user([{"type": "input_audio", "input_audio": {"format": "wav"}}])),
        "messages[0][0].input_audio.data",
    ),
    "bad base64": (
        body(user([{"type": "input_audio", "input_audio": {"data": "***", "format": "wav"}}])),
        "messages[0][0].input_audio.data",
    ),
    "base64 that is not a wav": (
        body(user([{"type": "input_audio", "input_audio": {"data": "aGVsbG8=", "format": "wav"}}])),
        "messages[0][0].input_audio.data",
    ),
    "too much audio": (
        body(user([audio_part(samples=160_000) for _ in range(31)])),
        "messages[0][30]",
    ),
    "text with the audio placeholder": (body(user(f"a{AUDIO_TOKEN_TEXT}")), "messages[0]"),
    "tool arguments not json": (
        body(
            user("hi"),
            {
                "role": "assistant",
                "tool_calls": [{**CALL, "function": {"name": "f", "arguments": "{"}}],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "x"},
        ),
        "messages[1].tool_calls[0].function.arguments",
    ),
    "tool arguments not an object": (
        body(
            user("hi"),
            {
                "role": "assistant",
                "tool_calls": [{**CALL, "function": {"name": "f", "arguments": "[1]"}}],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "x"},
        ),
        "messages[1].tool_calls[0].function.arguments",
    ),
    "tool result of an unknown call": (
        body(user("hi"), {"role": "tool", "tool_call_id": "nope", "content": "x"}),
        "messages[1].tool_call_id",
    ),
    "bad tool name": (
        body(user("hi"), tools=[{"type": "function", "function": {"name": 'a"b'}}]),
        "tools[0].function.name",
    ),
    "tool is not a function": (body(user("hi"), tools=[{"type": "retrieval"}]), "tools[0]"),
    "max_tokens zero": (body(user("hi"), max_tokens=0), "max_tokens"),
    "max_tokens bool": (body(user("hi"), max_tokens=True), "max_tokens"),
    "temperature too high": (body(user("hi"), temperature=3), "temperature"),
    "top_p zero": (body(user("hi"), top_p=0), "top_p"),
}


@pytest.mark.parametrize(("payload", "param"), BAD_REQUESTS.values(), ids=BAD_REQUESTS.keys())
def test_bad_requests_are_rejected_with_the_field_named(payload, param) -> None:
    with pytest.raises(RequestError) as caught:
        parse_request(payload)
    assert caught.value.param == param
    error = caught.value.body()["error"]
    assert error["type"] == "invalid_request_error"
    assert error["param"] == param
    assert error["message"]


def test_the_chat_template_silently_drops_a_part_it_does_not_know(glimmer_tokenizer) -> None:
    messages = [user([{"type": "input_audio", "clip": 0}, {"type": "text", "text": "kept"}])]
    text = glimmer_tokenizer.render_chat(messages, [])
    assert "kept" in text
    assert "input_audio" not in text


def test_prompt_text_refuses_a_part_the_template_would_drop(glimmer_tokenizer) -> None:
    request = ChatRequest(
        [user([{"type": "image", "image": "x"}, {"type": "text", "text": "a"}])],
        [],
        [],
        8,
        0.0,
        1.0,
        False,
    )
    with pytest.raises(PromptError, match="only text parts"):
        prompt_text(glimmer_tokenizer.render_chat, request, [])


def test_prompt_text_refuses_a_template_that_loses_a_placeholder() -> None:
    request = parse_request(body(user([audio_part()])))
    with pytest.raises(PromptError, match="placeholders"):
        prompt_text(lambda messages, tools: "no placeholders here", request, [4])


def test_several_clips_become_placeholder_runs_in_order(glimmer_tokenizer) -> None:
    request = parse_request(
        body(
            {"role": "system", "content": "You are Thalvi."},
            user([audio_part(), {"type": "text", "text": " then "}, audio_part(seed=1)]),
        )
    )
    text = prompt_text(glimmer_tokenizer.render_chat, request, [3, 5])
    assert (
        f"<|start|>user<|message|>{AUDIO_TOKEN_TEXT * 3} then {AUDIO_TOKEN_TEXT * 5}<|eot|>" in text
    )
    assert text.endswith("<|start|>assistant")
    assert "Reasoning strength: low." in text
    ids = glimmer_tokenizer.text_ids(text)
    assert ids.count(AUDIO_TOKEN_ID) == 8


def test_a_voice_turn_after_text_turns_renders_each_message_once(glimmer_tokenizer) -> None:
    request = parse_request(
        body(
            {"role": "system", "content": "Be brief."},
            user("first"),
            {"role": "assistant", "content": "one"},
            user([audio_part()]),
        )
    )
    text = prompt_text(glimmer_tokenizer.render_chat, request, [2])
    assert text.endswith(
        "<|start|>user<|message|>first<|eot|>"
        "<|start|>assistant to=user<|message|>one<|eot|>"
        f"<|start|>user<|message|>{AUDIO_TOKEN_TEXT * 2}<|eot|><|start|>assistant"
    )


def test_a_tool_round_trip_renders_as_the_template_writes_it(glimmer_tokenizer) -> None:
    request = parse_request(
        body(
            user("Weather in Paris?"),
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": "Look.",
                "tool_calls": [CALL],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "sunny"},
            tools=[WEATHER],
        )
    )
    text = prompt_text(glimmer_tokenizer.render_chat, request, [])
    assert '"name": "get_weather", "description": "Weather in a city"' in text
    assert '# Valid recipients: "self", "get_weather.*", "user".' in text
    assert text.endswith(
        "<|start|>assistant to=self<|message|>Look.<|eom|>"
        "<|start|>assistant to=get_weather<|message|><atem:function_calls>\n"
        '<atem:invoke name="get_weather">\n'
        '<atem:parameter name="city">Paris</atem:parameter>\n'
        '<atem:parameter name="days">3</atem:parameter>\n'
        '<atem:parameter name="ok">true</atem:parameter>\n'
        "</atem:invoke>\n</atem:function_calls><|eot|>"
        '<|start|>tool get_weather<|message|><tool_output name="get_weather">\nsunny\n'
        "</tool_output><|eot|><|start|>assistant"
    )


def test_a_tool_result_may_carry_the_name_instead_of_a_call_id(glimmer_tokenizer) -> None:
    request = parse_request(
        body(
            user("hi"),
            {"role": "tool", "name": "get_weather", "content": [{"type": "text", "text": "rain"}]},
        )
    )
    assert "<|start|>tool get_weather<|message|>" in prompt_text(
        glimmer_tokenizer.render_chat, request, []
    )


def test_parsing_does_not_change_the_body() -> None:
    payload = body(user([audio_part()]), tools=[WEATHER])
    before = copy.deepcopy(payload)
    parse_request(payload)
    assert payload == before
