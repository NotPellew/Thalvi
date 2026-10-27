import json

import pytest

from thalvi_train.reply import (
    Content,
    Event,
    IncrementalDecoder,
    Reasoning,
    ReplyParser,
    ToolCall,
    parse_tool_calls,
    tool_parameter_types,
)

WEATHER = [
    {
        "type": "function",
        "function": {
            "name": "get_weather",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string"},
                    "days": {"type": "integer"},
                    "tag": {"type": ["string", "null"]},
                    "units": {"enum": ["c", "f"]},
                },
            },
        },
    },
    {"type": "function", "function": {"name": "ping"}},
]
TYPES = tool_parameter_types(WEATHER)


def atem(name: str, *parameters: tuple[str, str]) -> str:
    inner = "".join(f'<atem:parameter name="{k}">{v}</atem:parameter>\n' for k, v in parameters)
    return (
        f'<atem:function_calls>\n<atem:invoke name="{name}">\n{inner}'
        "</atem:invoke>\n</atem:function_calls>"
    )


def merged(events: list[Event]) -> list[Event]:
    out: list[Event] = []
    for event in events:
        if out and isinstance(event, Reasoning | Content) and type(out[-1]) is type(event):
            out[-1] = type(event)(out[-1].text + event.text)
        else:
            out.append(event)
    return out


def run(text: str, size: int | None = None) -> tuple[list[Event], bool]:
    parser = ReplyParser(TYPES)
    pieces = [text] if size is None else [text[i : i + size] for i in range(0, len(text), size)]
    events = [event for piece in pieces for event in parser.feed(piece)]
    events += parser.finish()
    return merged(events), parser.done


ANSWER = " to=user<|message|>It is sunny.<|eot|>"
REASONED = " to=self<|message|>The user wants weather.<|eom|><|start|>assistant to=user<|message|>"

CASES = {
    "answer only": (ANSWER, [Content("It is sunny.")], True),
    "reasoning then answer": (
        REASONED + "Sunny.<|eot|>",
        [Reasoning("The user wants weather."), Content("Sunny.")],
        True,
    ),
    "header without recipient is the user": ("<|message|>Hi<|eot|>", [Content("Hi")], True),
    "leading start marker": ("<|start|>assistant" + ANSWER, [Content("It is sunny.")], True),
    "text after eot is ignored": (
        ANSWER + "extra<|start|>assistant to=user<|message|>no",
        [Content("It is sunny.")],
        True,
    ),
    "unknown special tokens are dropped": (
        " to=user<|message|>a<|reserved_special_token_7|>b<|eot|>",
        [Content("ab")],
        True,
    ),
    "angle brackets are text": (
        " to=user<|message|>1 < 2 and <b>x</b><|eot|>",
        [Content("1 < 2 and <b>x</b>")],
        True,
    ),
    "cut off in the answer": (
        REASONED + "Sun",
        [Reasoning("The user wants weather."), Content("Sun")],
        False,
    ),
    "cut off in a marker": (" to=user<|message|>Sun<|e", [Content("Sun")], False),
    "empty generation": ("", [], False),
    "immediate eot": ("<|eot|>", [], True),
    "tool call, last message": (
        " to=get_weather<|message|>" + atem("get_weather", ("city", "Paris")) + "<|eot|>",
        [ToolCall("get_weather", {"city": "Paris"})],
        True,
    ),
    "reasoning then tool call": (
        " to=self<|message|>Look it up.<|eom|><|start|>assistant to=get_weather<|message|>"
        + atem("get_weather", ("city", "Paris"), ("days", "3"))
        + "<|eom|>",
        [Reasoning("Look it up."), ToolCall("get_weather", {"city": "Paris", "days": 3})],
        False,
    ),
    "two calls, one after the other": (
        " to=get_weather<|message|>"
        + atem("get_weather", ("city", "Paris"))
        + "<|eom|><|start|>assistant to=ping<|message|>"
        + atem("ping")
        + "<|eot|>",
        [ToolCall("get_weather", {"city": "Paris"}), ToolCall("ping", {})],
        True,
    ),
    "two invokes in one block": (
        " to=get_weather<|message|><atem:function_calls>\n"
        '<atem:invoke name="ping">\n</atem:invoke>\n'
        '<atem:invoke name="ping">\n</atem:invoke>\n</atem:function_calls><|eot|>',
        [ToolCall("ping", {}), ToolCall("ping", {})],
        True,
    ),
    "unfinished tool call is dropped": (
        " to=get_weather<|message|>" + atem("get_weather", ("city", "Pa")),
        [],
        False,
    ),
    "malformed tool body yields nothing": (
        " to=get_weather<|message|>I will call it<|eot|>",
        [],
        True,
    ),
}


@pytest.mark.parametrize("size", [None, 1, 2, 3, 7])
@pytest.mark.parametrize(("text", "events", "done"), CASES.values(), ids=CASES.keys())
def test_parses_generated_text_however_it_is_cut_into_pieces(
    text: str, events: list[Event], done: bool, size: int | None
) -> None:
    assert run(text, size) == (events, done)


def test_special_token_text_never_reaches_a_delta() -> None:
    parser = ReplyParser(TYPES)
    text = REASONED + "Hello<|eot|>"
    deltas = [e for c in text for e in parser.feed(c)] + parser.finish()
    assert deltas
    assert all("<|" not in e.text for e in deltas if isinstance(e, Reasoning | Content))


def test_text_is_released_as_soon_as_it_cannot_start_a_marker() -> None:
    parser = ReplyParser()
    assert parser.feed(" to=user<|message|>Hel") == [Content("Hel")]
    assert parser.feed("lo <") == [Content("lo ")]
    assert parser.feed("|eo") == []
    assert parser.feed("t|>") == []
    assert parser.feed("<|") == []
    assert parser.feed("eot|>") == []
    assert parser.done


def test_a_lone_angle_bracket_at_the_end_is_text() -> None:
    parser = ReplyParser()
    parser.feed(" to=user<|message|>a <")
    assert parser.finish() == [Content("<")]


def test_a_cut_off_marker_is_not_text() -> None:
    parser = ReplyParser()
    parser.feed(" to=user<|message|>a <|ot")
    assert parser.finish() == []


@pytest.mark.parametrize(
    ("parameters", "expected"),
    [
        ([("city", "Paris")], {"city": "Paris"}),
        ([("city", "  New York \n")], {"city": "  New York \n"}),
        ([("city", "42")], {"city": "42"}),
        ([("city", "true")], {"city": "true"}),
        ([("city", "a</b> & <c>")], {"city": "a</b> & <c>"}),
        ([("city", "line one\nline two\n")], {"city": "line one\nline two\n"}),
        ([("days", "3")], {"days": 3}),
        ([("days", " 3 ")], {"days": 3}),
        ([("days", "three")], {"days": "three"}),
        ([("tag", "null")], {"tag": "null"}),
        ([("units", "f")], {"units": "f"}),
        ([("units", '"f"')], {"units": "f"}),
        ([("extra", "true")], {"extra": True}),
        ([("extra", "null")], {"extra": None}),
        ([("extra", "2.5")], {"extra": 2.5}),
        ([("extra", '["a", 1, {"b": null}]')], {"extra": ["a", 1, {"b": None}]}),
        ([("extra", '{"k": [1, 2]}')], {"extra": {"k": [1, 2]}}),
        ([("extra", "plain words")], {"extra": "plain words"}),
        ([("city", "Paris"), ("days", "2")], {"city": "Paris", "days": 2}),
        ([], {}),
    ],
)
def test_parameters_are_strings_when_declared_and_json_otherwise(
    parameters: list[tuple[str, str]], expected: dict
) -> None:
    (call,) = parse_tool_calls(atem("get_weather", *parameters), TYPES)
    assert call == ToolCall("get_weather", expected)


def test_a_tool_without_a_schema_still_parses_its_values() -> None:
    (call,) = parse_tool_calls(atem("unknown", ("a", "1"), ("b", "text")), {})
    assert call.arguments == {"a": 1, "b": "text"}


def test_arguments_are_a_json_string_that_keeps_unicode() -> None:
    call = ToolCall("f", {"city": "Zürich", "n": 1})
    assert json.loads(call.arguments_json()) == call.arguments
    assert "Zürich" in call.arguments_json()


@pytest.mark.parametrize(
    "text", ["Hello", "héllo wörld", "日本語のテキスト", "emoji 🎉 done", "<|eot|> mixed ✓"]
)
def test_incremental_decoding_never_splits_a_character(text: str) -> None:
    ids = list(text.encode())

    def decode(ids: list[int]) -> str:
        return bytes(ids).decode(errors="replace")

    decoder = IncrementalDecoder(decode)
    pieces = [decoder.push(i) for i in ids]
    assert "".join(pieces) + decoder.flush() == text
    assert all("�" not in piece for piece in pieces)


def test_incremental_decoding_flushes_dangling_bytes() -> None:
    decoder = IncrementalDecoder(lambda ids: bytes(ids).decode(errors="replace"))
    assert decoder.push("é".encode()[0]) == ""
    assert decoder.flush() == "�"
    assert decoder.flush() == ""
