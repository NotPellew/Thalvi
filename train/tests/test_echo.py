import pytest
from wav import audio_part

from thalvi_train.chat import parse_request
from thalvi_train.echo import MAX_ECHO_TOKENS, EchoWatch, ReplyMode, echo_end, uses_echo

TOOLS = [{"type": "function", "function": {"name": "f"}}]


@pytest.mark.parametrize(
    ("text", "echo"),
    [
        ("", None),
        (" \n ", None),
        (' "What is the', None),
        (" what is the weather", None),
        ("\n what is", None),
        (' what is "the" weather', None),
        (' "What is the weather?"', ' "What is the weather?"'),
        (' "What is the weather?"\n', ' "What is the weather?"'),
        (' "What is the weather?" And', ' "What is the weather?"'),
        (' "Hi" and "bye"', ' "Hi"'),
        (" “Hi there” and", " “Hi there”"),
        (" what is the weather\n\nWe", " what is the weather"),
        ('\n "Hi"', '\n "Hi"'),
        (' "What is\nthe weather?"', ' "What is'),
        (" what is it<|eom|>", " what is it"),
        (" what is it<|eot|>", " what is it"),
        ("<|eom|>", ""),
        (' "a"<|eom|>', ' "a"'),
    ],
)
def test_the_echo_ends_at_the_first_of_quote_line_break_or_marker(
    text: str, echo: str | None
) -> None:
    end = echo_end(text)
    assert (None if end is None else text[:end]) == echo


def run_watch(pieces: list[str]) -> tuple[str, int]:
    watch = EchoWatch()
    kept = ""
    for count, piece in enumerate(pieces, start=1):
        part, ended = watch.push(piece)
        kept += part
        if ended:
            return kept, count
    return kept, -1


@pytest.mark.parametrize(
    ("pieces", "kept", "ended_at"),
    [
        ([' "Hi', ' there?"', " More"], ' "Hi there?"', 2),
        ([' "Hi', ' there?"\n', "More"], ' "Hi there?"', 2),
        ([" Hi there", ".\n\n", "More"], " Hi there.", 2),
        ([" Hi", "", " there"], " Hi there", -1),
        ([" Hi", "<|eom|>"], " Hi", 2),
        (
            [" Hi"] + [" there"] * (MAX_ECHO_TOKENS + 5),
            " Hi" + " there" * (MAX_ECHO_TOKENS - 1),
            40,
        ),
    ],
)
def test_the_watch_keeps_the_echo_part_of_the_last_piece_and_caps_the_length(
    pieces: list[str], kept: str, ended_at: int
) -> None:
    assert run_watch(pieces) == (kept, ended_at)


def request(content, tools=None, *, history=()):
    body = {"stream": True, "messages": [*history, {"role": "user", "content": content}]}
    if tools:
        body["tools"] = tools
    return parse_request(body)


AUDIO = [audio_part()]
TEXT_AND_AUDIO = [{"type": "text", "text": "this:"}, *AUDIO]
EARLIER_AUDIO = [{"role": "user", "content": AUDIO}, {"role": "assistant", "content": "ok"}]


@pytest.mark.parametrize(
    ("chat", "mode", "echoes"),
    [
        (request(AUDIO), "echo", True),
        (request(TEXT_AND_AUDIO), "echo", True),
        (request(AUDIO, history=EARLIER_AUDIO), "echo", True),
        (request(AUDIO), "reasoning", False),
        (request("hello"), "echo", False),
        (request([{"type": "text", "text": "hello"}]), "echo", False),
        (request("hello", history=EARLIER_AUDIO), "echo", False),
        (request(AUDIO, TOOLS), "echo", False),
    ],
)
def test_only_a_spoken_turn_without_tools_echoes(chat, mode: ReplyMode, echoes: bool) -> None:
    assert uses_echo(chat, mode) is echoes
