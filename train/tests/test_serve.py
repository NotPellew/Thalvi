import asyncio
import itertools
import json
import threading
from collections.abc import Iterator
from pathlib import Path

import pytest
import torch
from starlette.testclient import TestClient
from tiny_glimmer import EOT, HIDDEN, START, FakeTokenizer
from tiny_whisper import DIM
from wav import audio_part

from thalvi_train import engine as engine_module
from thalvi_train.echo import HANDOFF, ReplyMode
from thalvi_train.engine import Engine
from thalvi_train.glimmer import AUDIO_TOKEN_ID, load_glimmer
from thalvi_train.projector import AudioProjector
from thalvi_train.prompts import AUDIO_TOKEN_TEXT
from thalvi_train.serve import MODEL_ID, _parse_args, create_app

MESSAGE = 200023
EOM = 200007
MARKERS = {"<|start|>": START, "<|message|>": MESSAGE, "<|eom|>": EOM, "<|eot|>": EOT}
MERGED_DOT_NEWLINE = 300
STACK = 2


class ChatFakeTokenizer(FakeTokenizer):
    """The fake tokenizer plus a chat rendering that keeps audio placeholders and text."""

    def render_chat(self, messages: list[dict], tools: list[dict]) -> str:
        def flat(content) -> str:
            if isinstance(content, str):
                return content
            return "".join(part["text"] for part in content)

        return "".join(f"{m['role']}:{flat(m['content'])}|" for m in messages)

    def text_ids(self, text: str) -> list[int]:
        ids: list[int] = []
        for i, part in enumerate(text.split(AUDIO_TOKEN_TEXT)):
            ids += [AUDIO_TOKEN_ID] * (i > 0) + script(part)
        return ids

    def decode(self, ids: list[int]) -> str:
        names = {value: key for key, value in MARKERS.items()} | {MERGED_DOT_NEWLINE: ".\n"}
        return "".join(chr(i) if i < 128 else names.get(i, f"<{i}>") for i in ids)


def script(text: str) -> list[int]:
    ids: list[int] = []
    while text:
        marker = next((m for m in MARKERS if text.startswith(m)), None)
        if marker:
            ids.append(MARKERS[marker])
            text = text[len(marker) :]
        else:
            ids.append(ord(text[0]))
            text = text[1:]
    return ids


class Scripted:
    """Stands in for `decode_steps`: yields the script's ids, then ends, counting the pulls.

    Tokens sent after a step are recorded in `forced`, and the script goes on with the
    next id as if the model had continued from them.
    """

    def __init__(self, ids: Iterator[int]) -> None:
        self.ids = ids
        self.pulled = 0
        self.forced: list[list[int]] = []
        self.closed = threading.Event()

    def __call__(self, glimmer, embeds, mask, max_new_tokens, stop_id, pick):
        try:
            for index, token in enumerate(self.ids):
                if index == max_new_tokens:
                    return
                self.pulled += 1
                forced = yield [token]
                if forced:
                    self.forced.append(forced)
                elif token == stop_id:
                    return
        finally:
            self.closed.set()


def make_engine(tiny_glimmer_dir: Path, tiny_encoder, mode: ReplyMode) -> Engine:
    torch.manual_seed(3)
    projector = AudioProjector(DIM, 24, HIDDEN, STACK).eval()
    encoder, extractor = tiny_encoder
    glimmer = load_glimmer(tiny_glimmer_dir, dtype=torch.float32)
    return Engine(glimmer, ChatFakeTokenizer(), projector, encoder, extractor, mode)


@pytest.fixture
def engine(tiny_glimmer_dir: Path, tiny_encoder) -> Engine:
    return make_engine(tiny_glimmer_dir, tiny_encoder, "reasoning")


@pytest.fixture
def client(engine: Engine) -> Iterator[TestClient]:
    with TestClient(create_app(engine)) as test_client:
        yield test_client


@pytest.fixture
def echo_client(tiny_glimmer_dir: Path, tiny_encoder) -> Iterator[TestClient]:
    engine = make_engine(tiny_glimmer_dir, tiny_encoder, "echo")
    with TestClient(create_app(engine)) as test_client:
        yield test_client


def request_body(*messages: dict, **extra) -> dict:
    return {"stream": True, "messages": list(messages), **extra}


def post(client: TestClient, payload: dict) -> tuple[list[str], list[dict]]:
    with client.stream("POST", "/v1/chat/completions", json=payload) as response:
        assert response.status_code == 200
        assert response.headers["content-type"].startswith("text/event-stream")
        raw = "".join(response.iter_text())
    frames = raw.split("\n\n")
    assert frames.pop() == ""
    assert all(frame.startswith("data: ") for frame in frames)
    data = [frame.removeprefix("data: ") for frame in frames]
    return data, [json.loads(d) for d in data if d != "[DONE]"]


def deltas(chunks: list[dict]) -> list[dict]:
    return [c["choices"][0]["delta"] for c in chunks if c["choices"]]


ANSWER = " to=self<|message|>User asks.<|eom|><|start|>assistant to=user<|message|>Hello!<|eot|>"


def test_streams_reasoning_then_answer_then_finish_usage_and_done(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(ANSWER))))
    data, chunks = post(
        client,
        request_body(
            {"role": "user", "content": [audio_part(samples=80_000)]},
            stream_options={"include_usage": True},
            reasoning_effort="minimal",
        ),
    )
    assert data[-1] == "[DONE]"
    assert {c["object"] for c in chunks} == {"chat.completion.chunk"}
    assert len({c["id"] for c in chunks}) == 1
    assert {c["model"] for c in chunks} == {MODEL_ID}
    pieces = deltas(chunks)
    assert pieces[0] == {"role": "assistant"}
    reasoning = [p for p in pieces if "reasoning_content" in p]
    content = [p for p in pieces if "content" in p]
    assert "".join(p["reasoning_content"] for p in reasoning) == "User asks."
    assert "".join(p["content"] for p in content) == "Hello!"
    assert pieces.index(reasoning[-1]) < pieces.index(content[0])
    assert all(len(p["content"]) == 1 for p in content)
    final, usage = chunks[-2], chunks[-1]
    assert final["choices"] == [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    assert usage["choices"] == []
    assert usage["usage"]["completion_tokens"] == len(script(ANSWER))
    assert usage["usage"]["prompt_tokens"] >= 250 // STACK
    assert usage["usage"]["total_tokens"] == (
        usage["usage"]["prompt_tokens"] + usage["usage"]["completion_tokens"]
    )


def test_without_include_usage_the_finish_chunk_is_the_last_before_done(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(ANSWER))))
    data, chunks = post(client, request_body({"role": "user", "content": "hi"}))
    assert chunks[-1]["choices"][0]["finish_reason"] == "stop"
    assert not any("usage" in c for c in chunks)
    assert data[-1] == "[DONE]"


def test_a_tool_call_is_streamed_whole_with_an_index_and_finishes_as_tool_calls(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = (
        " to=get_weather<|message|><atem:function_calls>\n"
        '<atem:invoke name="get_weather">\n'
        '<atem:parameter name="city">Paris</atem:parameter>\n'
        '<atem:parameter name="days">3</atem:parameter>\n'
        "</atem:invoke>\n</atem:function_calls><|eom|><|start|>assistant to=get_weather<|message|>"
        '<atem:function_calls>\n<atem:invoke name="get_weather">\n'
        '<atem:parameter name="city">Rome</atem:parameter>\n'
        "</atem:invoke>\n</atem:function_calls><|eot|>"
    )
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(calls))))
    tools = [
        {
            "type": "function",
            "function": {
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            },
        }
    ]
    _, chunks = post(client, request_body({"role": "user", "content": "weather?"}, tools=tools))
    sent = [p["tool_calls"] for p in deltas(chunks) if "tool_calls" in p]
    assert [len(s) for s in sent] == [1, 1]
    assert [s[0]["index"] for s in sent] == [0, 1]
    assert all(s[0]["type"] == "function" and s[0]["id"].startswith("call_") for s in sent)
    assert sent[0][0]["id"] != sent[1][0]["id"]
    assert [s[0]["function"]["name"] for s in sent] == ["get_weather", "get_weather"]
    assert [json.loads(s[0]["function"]["arguments"]) for s in sent] == [
        {"city": "Paris", "days": 3},
        {"city": "Rome"},
    ]
    assert chunks[-1]["choices"][0]["finish_reason"] == "tool_calls"
    assert not any("content" in p for p in deltas(chunks))


def test_hitting_max_tokens_finishes_as_length(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(ANSWER))))
    _, chunks = post(client, request_body({"role": "user", "content": "hi"}, max_tokens=30))
    assert chunks[-1]["choices"][0]["finish_reason"] == "length"


def test_the_tiny_model_itself_streams_until_the_token_limit(client: TestClient) -> None:
    data, chunks = post(
        client,
        request_body(
            {"role": "user", "content": [audio_part(), {"type": "text", "text": "?"}]},
            max_tokens=5,
            temperature=0.8,
            top_p=0.9,
            stream_options={"include_usage": True},
        ),
    )
    assert data[-1] == "[DONE]"
    assert chunks[-1]["usage"]["completion_tokens"] == 5
    assert chunks[-2]["choices"][0]["finish_reason"] == "length"


QUOTED = ' "Is it going to rain?"'
SPOKEN = [
    pytest.param(
        script(QUOTED + "No rain today.<|eot|>"),
        'The user said: "Is it going to rain?"',
        "No rain today.",
        [ord('"'), *script(HANDOFF)],
        id="closing-quote",
    ),
    pytest.param(
        [*script(" Rain today"), MERGED_DOT_NEWLINE, *script("No rain.<|eot|>")],
        "The user said: Rain today.",
        "No rain.",
        [ord("."), *script(HANDOFF)],
        id="line-break-inside-a-token",
    ),
    pytest.param(
        script(" Rain today<|eom|>No rain.<|eot|>"),
        "The user said: Rain today",
        "No rain.",
        script(HANDOFF),
        id="the-model-ends-the-message",
    ),
    pytest.param(
        script(QUOTED + QUOTED + "<|eot|>"),
        'The user said: "Is it going to rain?"',
        QUOTED,
        [ord('"'), *script(HANDOFF)],
        id="the-answer-repeats-the-echo",
    ),
]


@pytest.mark.parametrize(("tokens", "reasoning", "content", "forced"), SPOKEN)
def test_a_spoken_turn_streams_the_echo_as_reasoning_then_the_answer(
    echo_client: TestClient,
    monkeypatch: pytest.MonkeyPatch,
    tokens: list[int],
    reasoning: str,
    content: str,
    forced: list[int],
) -> None:
    fake = Scripted(iter(tokens))
    monkeypatch.setattr(engine_module, "decode_steps", fake)
    _, chunks = post(
        echo_client,
        request_body(
            {"role": "user", "content": [audio_part()]}, stream_options={"include_usage": True}
        ),
    )
    pieces = deltas(chunks)
    last_reasoning = max(i for i, p in enumerate(pieces) if "reasoning_content" in p)
    first_content = min(i for i, p in enumerate(pieces) if "content" in p)
    assert last_reasoning < first_content
    assert "".join(p.get("reasoning_content", "") for p in pieces) == reasoning
    assert "".join(p.get("content", "") for p in pieces) == content
    assert fake.forced == [forced]
    assert chunks[-2]["choices"][0]["finish_reason"] == "stop"
    assert chunks[-1]["usage"]["prompt_tokens"] >= len(script(HANDOFF))


@pytest.mark.parametrize(
    "message",
    [
        {"role": "user", "content": "hi"},
        {"role": "user", "content": [audio_part()]},
    ],
    ids=["text", "audio-with-tools"],
)
def test_text_turns_and_requests_with_tools_keep_the_reasoning(
    echo_client: TestClient, monkeypatch: pytest.MonkeyPatch, message: dict
) -> None:
    fake = Scripted(iter(script(ANSWER)))
    monkeypatch.setattr(engine_module, "decode_steps", fake)
    tools = [] if message["content"] == "hi" else [{"type": "function", "function": {"name": "f"}}]
    _, chunks = post(echo_client, request_body(message, tools=tools))
    pieces = deltas(chunks)
    assert "".join(p.get("reasoning_content", "") for p in pieces) == "User asks."
    assert "".join(p.get("content", "") for p in pieces) == "Hello!"
    assert fake.forced == []


def test_the_tiny_model_goes_from_echo_to_answer_through_one_cache(echo_client: TestClient) -> None:
    _, chunks = post(
        echo_client,
        request_body(
            {"role": "user", "content": [audio_part()]},
            max_tokens=60,
            stream_options={"include_usage": True},
        ),
    )
    pieces = deltas(chunks)
    reasoning = "".join(p.get("reasoning_content", "") for p in pieces)
    assert reasoning.startswith("The user said:")
    assert any("content" in p for p in pieces)
    assert chunks[-2]["choices"][0]["finish_reason"] == "length"
    assert chunks[-1]["usage"]["completion_tokens"] == 60


def test_the_reply_mode_flag_defaults_to_echo() -> None:
    required = ["--glimmer", "g", "--projector", "p"]
    assert _parse_args(required).reply_mode == "echo"
    assert _parse_args([*required, "--reply-mode", "reasoning"]).reply_mode == "reasoning"


@pytest.mark.parametrize(
    ("payload", "param"),
    [
        ({"stream": True, "messages": []}, "messages"),
        ({"messages": [{"role": "user", "content": "hi"}]}, "stream"),
        (request_body({"role": "user", "content": [{"type": "video"}]}), "messages[0][0].type"),
    ],
)
def test_a_bad_request_is_a_400_with_an_openai_error_and_no_stream(
    client: TestClient, payload: dict, param: str
) -> None:
    response = client.post("/v1/chat/completions", json=payload)
    assert response.status_code == 400
    assert response.headers["content-type"] == "application/json"
    assert response.json()["error"]["param"] == param
    assert response.json()["error"]["type"] == "invalid_request_error"


def test_a_body_that_is_not_json_is_a_400(client: TestClient) -> None:
    response = client.post("/v1/chat/completions", content=b"{nope")
    assert response.status_code == 400
    assert response.json()["error"]["type"] == "invalid_request_error"


def test_a_failure_before_the_first_token_is_a_500_not_a_stream(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    def broken(*args, **kwargs):
        raise RuntimeError("out of memory")
        yield

    monkeypatch.setattr(engine_module, "decode_steps", broken)
    response = client.post(
        "/v1/chat/completions", json=request_body({"role": "user", "content": "hi"})
    )
    assert response.status_code == 500
    assert response.json()["error"] == {
        "message": "out of memory",
        "type": "server_error",
        "param": None,
        "code": None,
    }


def test_a_failure_mid_stream_closes_it_without_done_or_error_text(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    def failing(*args, **kwargs):
        for token in script(" to=user<|message|>Hel"):
            yield [token]
        raise RuntimeError("cuda error")

    monkeypatch.setattr(engine_module, "decode_steps", failing)
    data, chunks = post(client, request_body({"role": "user", "content": "hi"}))
    assert "".join(p.get("content", "") for p in deltas(chunks)) == "Hel"
    assert "[DONE]" not in data
    assert not any(
        "finish_reason" in c["choices"][0] and c["choices"][0]["finish_reason"] for c in chunks
    )
    assert "cuda" not in "".join(data)


def test_the_next_request_runs_after_one_failed(
    client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(ANSWER))))
    payload = request_body({"role": "user", "content": "hi"})
    post(client, payload)
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(ANSWER))))
    _, chunks = post(client, payload)
    assert chunks[-1]["choices"][0]["finish_reason"] == "stop"


async def drive(app, payload: dict, disconnect_after_frames: int) -> list[dict]:
    """Calls the ASGI app directly; the client goes away once it has received that many frames."""
    sent: list[dict] = []
    pending = [{"type": "http.request", "body": json.dumps(payload).encode(), "more_body": False}]

    async def receive() -> dict:
        if pending:
            return pending.pop(0)
        if len(sent) > disconnect_after_frames:
            return {"type": "http.disconnect"}
        await asyncio.Event().wait()
        raise AssertionError

    async def send(message: dict) -> None:
        sent.append(message)

    scope = {
        "type": "http",
        "asgi": {"version": "3.0", "spec_version": "2.4"},
        "http_version": "1.1",
        "method": "POST",
        "path": "/v1/chat/completions",
        "raw_path": b"/v1/chat/completions",
        "query_string": b"",
        "headers": [(b"content-type", b"application/json")],
    }
    await asyncio.wait_for(app(scope, receive, send), timeout=30)
    return sent


def test_generation_stops_when_the_client_disconnects(
    engine: Engine, monkeypatch: pytest.MonkeyPatch
) -> None:
    prefix = script(" to=user<|message|>")
    endless = Scripted(itertools.chain(prefix, itertools.repeat(ord("a"))))
    monkeypatch.setattr(engine_module, "decode_steps", endless)
    payload = request_body({"role": "user", "content": "hi"}, max_tokens=10**7)
    sent = asyncio.run(drive(create_app(engine), payload, disconnect_after_frames=6))
    assert endless.closed.is_set()
    assert endless.pulled < 10**7
    body = b"".join(m.get("body", b"") for m in sent)
    assert b"[DONE]" not in body
    assert b'"finish_reason":"' not in body
    monkeypatch.setattr(engine_module, "decode_steps", Scripted(iter(script(ANSWER))))
    done = asyncio.run(
        drive(create_app(engine), request_body({"role": "user", "content": "x"}), 10**6)
    )
    assert b"[DONE]" in b"".join(m.get("body", b"") for m in done)


@pytest.fixture
def keyed_client(engine: Engine) -> Iterator[TestClient]:
    with TestClient(create_app(engine, api_key="secret")) as test_client:
        yield test_client


def test_models_routes_list_the_served_model(client: TestClient) -> None:
    for path in ("/models", "/v1/models"):
        body = client.get(path).json()
        assert body["object"] == "list"
        assert [entry["id"] for entry in body["data"]] == [MODEL_ID]


@pytest.mark.parametrize("path", ["/models", "/v1/models"])
@pytest.mark.parametrize("authorization", [None, "Bearer wrong", "secret", "Bearer secret2"])
def test_a_missing_or_wrong_key_is_a_401_on_every_route(
    keyed_client: TestClient, path: str, authorization: str | None
) -> None:
    headers = {} if authorization is None else {"Authorization": authorization}
    response = keyed_client.get(path, headers=headers)
    assert response.status_code == 401
    assert response.json()["error"]["type"] == "invalid_request_error"
    assert response.headers["www-authenticate"] == "Bearer"


def test_a_chat_request_without_the_key_is_a_401_and_with_it_streams(
    keyed_client: TestClient,
) -> None:
    payload = request_body({"role": "user", "content": "hi"}, max_tokens=2)
    assert keyed_client.post("/v1/chat/completions", json=payload).status_code == 401
    headers = {"Authorization": "Bearer secret"}
    with keyed_client.stream(
        "POST", "/v1/chat/completions", json=payload, headers=headers
    ) as response:
        assert response.status_code == 200
        assert response.read().decode().endswith("data: [DONE]\n\n")
    assert keyed_client.get("/v1/models", headers=headers).status_code == 200


def test_without_a_key_no_auth_is_asked(client: TestClient) -> None:
    assert client.get("/v1/models").status_code == 200


def test_the_api_key_flag_defaults_to_the_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    required = ["--glimmer", "g", "--projector", "p"]
    monkeypatch.delenv("THALVI_ENDPOINT_KEY", raising=False)
    assert _parse_args(required).api_key is None
    monkeypatch.setenv("THALVI_ENDPOINT_KEY", "from-env")
    assert _parse_args(required).api_key == "from-env"
    assert _parse_args([*required, "--api-key", "k"]).api_key == "k"


def test_warming_up_runs_one_short_spoken_request_and_leaves_the_engine_free(
    engine: Engine, monkeypatch: pytest.MonkeyPatch
) -> None:
    limits: list[int] = []
    real = engine_module.decode_steps

    def counted(glimmer, embeds, mask, max_new_tokens, *rest):
        limits.append(max_new_tokens)
        return real(glimmer, embeds, mask, max_new_tokens, *rest)

    monkeypatch.setattr(engine_module, "decode_steps", counted)
    engine.warm_up()
    engine.warm_up()

    assert limits == [engine_module.WARM_UP_TOKENS] * 2
