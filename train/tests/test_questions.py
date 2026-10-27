import asyncio
import json
from collections.abc import Callable
from pathlib import Path

import httpx
import pytest

from thalvi_train import questions
from thalvi_train.questions import (
    ANGLES,
    MODEL,
    TOPICS,
    QuestionsError,
    Settings,
    build_prompt,
    generate,
    main,
    parse_requests,
    read_questions,
)

Handler = Callable[[httpx.Request], httpx.Response]


def completion(content: str | None, tokens: int = 100) -> httpx.Response:
    message = {"role": "assistant", "content": content, "reasoning_content": "thinking"}
    return httpx.Response(
        200, json={"choices": [{"message": message}], "usage": {"total_tokens": tokens}}
    )


def numbered(prefix: str, count: int = 25) -> str:
    return json.dumps([f"{prefix} request number {i}" for i in range(count)])


class Server:
    def __init__(self, handler: Handler) -> None:
        self.handler = handler
        self.requests: list[httpx.Request] = []
        self.delays: list[float] = []

    def transport(self) -> httpx.MockTransport:
        def respond(request: httpx.Request) -> httpx.Response:
            self.requests.append(request)
            return self.handler(request)

        return httpx.MockTransport(respond)

    async def sleep(self, seconds: float) -> None:
        self.delays.append(seconds)

    def run(self, out: Path, **overrides: object) -> tuple[int, int]:
        defaults = {"target": 50, "concurrency": 2}
        settings = Settings(api_key="secret-key", **{**defaults, **overrides})
        return asyncio.run(generate(settings, out, transport=self.transport(), sleep=self.sleep))


def distinct_batches() -> Handler:
    calls = iter(range(10_000))
    return lambda _request: completion(numbered(f"call{next(calls)}"), tokens=1000)


@pytest.mark.parametrize(
    "content",
    [
        '["a b", "c"]',
        'Sure! Here you go:\n```json\n["a b", "c"]\n```\nEnjoy.',
        '```\n["a b", "c"]\n```',
        'Here: ["a b", "  c  ", ""]',
    ],
)
def test_parse_requests_finds_the_array(content: str) -> None:
    assert parse_requests(content) == ["a b", "c"]


@pytest.mark.parametrize("content", ["no array", "[1, 2]", '{"a": 1}', '["unclosed', "[not json]"])
def test_parse_requests_rejects_bad_replies(content: str) -> None:
    with pytest.raises(questions.BadCall):
        parse_requests(content)


def test_every_topic_has_the_planned_angles() -> None:
    assert all(len(angles) == ANGLES for angles in TOPICS.values())


def test_prompt_asks_for_the_batch_size_and_excludes_health() -> None:
    prompt = build_prompt("cooking", TOPICS["cooking"][0], 25, 2)
    assert "Write 25 distinct" in prompt
    assert "batch 3" in prompt
    assert "nutrition, diet" in prompt


def test_collects_the_target_with_the_documented_request(tmp_path: Path) -> None:
    server = Server(distinct_batches())
    out = tmp_path / "q.jsonl"
    total, tokens = server.run(out)
    rows = [json.loads(line) for line in out.read_text().splitlines()]
    assert total == len(rows) == 50
    assert tokens == 2000
    assert {row["topic"] for row in rows} <= set(TOPICS)
    request = server.requests[0]
    body = json.loads(request.content)
    assert str(request.url) == "https://inference.baseten.co/v1/chat/completions"
    assert request.headers["authorization"] == "Bearer secret-key"
    assert body["model"] == MODEL == "meta-models/muse-spark-open-1026"
    assert body["max_tokens"] == 4096


def test_dedupes_case_insensitively_across_calls_and_runs(tmp_path: Path) -> None:
    out = tmp_path / "q.jsonl"
    replies = iter(
        [
            json.dumps(["Turn it up", "turn  it UP", "Call mom"]),
            json.dumps(["call mom", "Play jazz"]),
        ]
    )
    server = Server(lambda _request: completion(next(replies, "[]")))
    server.run(out, topics=("general",), concurrency=1, max_total_tokens=250)
    assert [q.text for q in read_questions(out)] == ["Turn it up", "Call mom", "Play jazz"]


def test_resumes_by_appending_to_existing_rows(tmp_path: Path) -> None:
    out = tmp_path / "q.jsonl"
    out.write_text(json.dumps({"text": "Already here", "topic": "road"}) + "\n")
    replies = iter([json.dumps(["Already HERE", "Fresh one"])])
    server = Server(lambda _request: completion(next(replies)))
    total, _ = server.run(out, target=2, concurrency=1)
    assert total == 2
    assert [q.text for q in read_questions(out)] == ["Already here", "Fresh one"]


def test_stops_at_the_token_budget(tmp_path: Path) -> None:
    server = Server(distinct_batches())
    total, tokens = server.run(
        tmp_path / "q.jsonl", target=10_000, concurrency=1, max_total_tokens=2500
    )
    assert (tokens, total) == (3000, 75)
    assert len(server.requests) == 3


def test_null_content_fails_the_call_but_still_counts_its_tokens(tmp_path: Path) -> None:
    replies = iter([completion(None, tokens=4443), completion(numbered("ok"), tokens=900)])
    server = Server(lambda _request: next(replies))
    total, tokens = server.run(tmp_path / "q.jsonl", target=25, concurrency=1)
    assert (total, tokens) == (25, 5343)


@pytest.mark.parametrize("status", [429, 503])
def test_retries_throttling_and_server_errors_with_exponential_backoff(
    tmp_path: Path, status: int
) -> None:
    replies = iter([httpx.Response(status), httpx.Response(status), completion(numbered("ok"))])
    server = Server(lambda _request: next(replies))
    total, _ = server.run(tmp_path / "q.jsonl", target=25, concurrency=1)
    assert total == 25
    assert server.delays == [1.0, 2.0]


def test_a_rejected_key_stops_the_run(tmp_path: Path) -> None:
    server = Server(lambda _request: httpx.Response(401, text="bad key"))
    with pytest.raises(QuestionsError, match="HTTP 401"):
        server.run(tmp_path / "q.jsonl")
    assert len(server.requests) <= 2


def test_gives_up_after_repeated_failed_calls(tmp_path: Path) -> None:
    server = Server(lambda _request: completion("not an array"))
    with pytest.raises(QuestionsError, match="in a row"):
        server.run(tmp_path / "q.jsonl", concurrency=1)
    assert len(server.requests) == questions.MAX_CONSECUTIVE_FAILURES


def test_main_requires_the_key_in_the_environment(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.delenv("BASETEN_API_KEY", raising=False)
    with pytest.raises(SystemExit):
        main([str(tmp_path / "q.jsonl")])
