"""Writes spoken-style assistant requests with Muse Spark Open on Baseten.

Run as `python -m thalvi_train.questions <out.jsonl>`.
Output is JSONL of `{"text", "topic"}`, appended call by call so a run can resume.
The API key comes from `BASETEN_API_KEY` in the environment and nowhere else.
"""

import argparse
import asyncio
import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

import httpx

from thalvi_train.http_retry import RETRYABLE_STATUS, Sleep, post_with_retry

BASE_URL = "https://inference.baseten.co/v1"
MODEL = "meta-models/muse-spark-open-1026"
MAX_TOKENS = 4096
REASONING_EFFORT = "low"
CALL_TIMEOUT_SECONDS = 600.0
MAX_CONSECUTIVE_FAILURES = 10
ANGLES = 4

TOPICS: dict[str, tuple[str, ...]] = {
    "cooking": (
        "asking what to cook from a few ingredients left in the fridge or pantry",
        "step-by-step help while a recipe is under way, including setting and checking timers",
        "swapping ingredients, scaling a recipe and fixing a dish that went wrong",
        "kitchen techniques, temperatures and times for baking, roasting and boiling",
    ),
    "road": (
        "asking for a short news briefing while driving",
        "follow-up questions on a news story that was just read out",
        "directions, detours and where to stop on a trip",
        "traffic, arrival times and parking",
    ),
    "messages": (
        "hearing new text messages read aloud",
        "dictating a reply to a text message",
        "dictating and revising a short email, including who it is for",
        "asking what is new in the inbox and replying to a specific person",
    ),
    "fixing": (
        "repairing a household item such as a tap, a bike or a door",
        "assembling furniture or gadgets from a box of parts",
        "choosing tools, screws and materials for a small job",
        "working out why a device or appliance misbehaves",
    ),
    "errands": (
        "building and changing a shopping list",
        "opening hours, nearby shops and what to pick up on the way home",
        "returns, deliveries, appointments and bills",
        "comparing two things to buy and deciding between them",
    ),
    "general": (
        "curious questions about science, history and the world",
        "small talk, jokes and opinions in a friendly chat",
        "explaining a word, a place, a person or how something works",
        "recommendations for music, films, books and weekend plans",
    ),
    "quick-facts": (
        "unit and currency conversions",
        "mental arithmetic, percentages, tips and splitting a bill",
        "dates, time zones and how long until something",
        "short factual lookups with a one-line answer",
    ),
}

_FENCE = re.compile(r"```(?:json)?\s*(.*?)```", re.DOTALL)


class QuestionsError(RuntimeError):
    """The run cannot continue, for example after a rejected key or repeated failures."""


class BadCall(Exception):
    """A call or its reply is unusable; the run goes on with the next call."""


@dataclass(frozen=True)
class Question:
    text: str
    topic: str


@dataclass(frozen=True)
class Settings:
    api_key: str
    target: int
    batch_size: int = 25
    concurrency: int = 8
    max_total_tokens: int = 3_000_000
    topics: tuple[str, ...] = tuple(TOPICS)
    base_url: str = BASE_URL
    retries: int = 5
    base_delay: float = 1.0


def build_prompt(topic: str, angle: str, batch_size: int, batch: int) -> str:
    """The user message asking for `batch_size` requests about one angle of one topic."""
    return (
        f"Write {batch_size} distinct things a person might say out loud to a hands-free voice "
        f"assistant. Topic: {topic.replace('-', ' ')}. Angle: {angle}.\n\n"
        "Rules:\n"
        "- Phrase each one the way people really talk: contractions, the odd filler like 'um' or "
        "'uh', now and then a self-correction ('no wait, I mean ...').\n"
        "- Write every number as spoken words ('twenty minutes', 'a hundred and fifty grams'), "
        "never digits, and no symbols or emoji.\n"
        "- Vary the length a lot, from 3 to 40 words; mix short commands with longer, "
        "rambling ones.\n"
        "- Some are questions, some are commands, some are statements that need a reply.\n"
        "- Use specific, varied details (names, places, foods, objects) and never repeat "
        "yourself.\n"
        "- Say nothing about nutrition, diet, calories or medical advice.\n"
        f"- This is batch {batch + 1} for this angle, so avoid the most obvious requests.\n\n"
        "Reply with only a JSON array of strings."
    )


def parse_requests(content: str) -> list[str]:
    """The strings of the JSON array in `content`, which may sit inside a code fence."""
    fenced = _FENCE.search(content)
    body = fenced.group(1) if fenced else content
    start, end = body.find("["), body.rfind("]")
    if start < 0 or end < start:
        raise BadCall("no JSON array in the reply")
    try:
        items = json.loads(body[start : end + 1])
    except json.JSONDecodeError as error:
        raise BadCall(f"reply is not valid JSON: {error}") from error
    if not isinstance(items, list) or not all(isinstance(item, str) for item in items):
        raise BadCall("reply is not an array of strings")
    return [item.strip() for item in items if item.strip()]


def dedupe_key(text: str) -> str:
    """Case-insensitive, whitespace-insensitive identity of a request."""
    return " ".join(text.casefold().split())


def read_questions(path: Path) -> list[Question]:
    """Questions already in `path`, or none if it does not exist."""
    if not path.exists():
        return []
    questions = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
            questions.append(Question(text=row["text"], topic=row["topic"]))
        except (json.JSONDecodeError, KeyError, TypeError) as error:
            raise QuestionsError(f"{path}:{number}: bad question row: {error!r}") from error
    return questions


def parse_completion(response: httpx.Response) -> tuple[str | None, int]:
    """The message content and total tokens of a Chat Completions response.

    `content` is `None` when the model spent `max_tokens` reasoning; the tokens
    were still spent and still count against the budget.
    """
    try:
        body = response.json()
        content = body["choices"][0]["message"]["content"]
        tokens = int(body.get("usage", {}).get("total_tokens", 0))
    except (ValueError, KeyError, IndexError, TypeError, AttributeError) as error:
        raise BadCall(f"unexpected response shape: {error!r}") from error
    if content is not None and not isinstance(content, str):
        raise BadCall("message content is neither text nor null")
    return content, tokens


class _Run:
    def __init__(
        self, settings: Settings, client: httpx.AsyncClient, out: Path, sleep: Sleep
    ) -> None:
        self.settings = settings
        self.client = client
        self.out = out
        self.sleep = sleep
        existing = read_questions(out)
        self.seen = {dedupe_key(q.text) for q in existing}
        self.count = len(existing)
        self.tokens = 0
        self.failures = 0
        self.next_call = self.count // settings.batch_size
        self.plan = [(t, TOPICS[t][i]) for i in range(ANGLES) for t in settings.topics]

    def done(self) -> bool:
        return self.count >= self.settings.target or self.tokens >= self.settings.max_total_tokens

    async def worker(self) -> None:
        while not self.done():
            call = self.next_call
            self.next_call += 1
            topic, angle = self.plan[call % len(self.plan)]
            await self.one_call(topic, angle, call // len(self.plan))

    async def one_call(self, topic: str, angle: str, batch: int) -> None:
        settings = self.settings
        prompt = build_prompt(topic, angle, settings.batch_size, batch)
        try:
            response = await post_with_retry(
                self.client,
                f"{settings.base_url}/chat/completions",
                json={
                    "model": MODEL,
                    "max_tokens": MAX_TOKENS,
                    "reasoning_effort": REASONING_EFFORT,
                    "messages": [{"role": "user", "content": prompt}],
                },
                headers={"Authorization": f"Bearer {settings.api_key}"},
                retries=settings.retries,
                base_delay=settings.base_delay,
                sleep=self.sleep,
            )
            status = response.status_code
            if status in RETRYABLE_STATUS:
                raise BadCall(f"HTTP {status} after {settings.retries} retries")
            if status >= 400:
                raise QuestionsError(f"HTTP {status}: {response.text[:300]}")
            content, tokens = parse_completion(response)
            self.tokens += tokens
            if content is None:
                raise BadCall("the model returned no content (reasoning used the token limit)")
            requests = parse_requests(content)
        except (httpx.TransportError, BadCall) as error:
            self.fail(error)
            return
        self.failures = 0
        self.add(topic, requests)

    def fail(self, error: Exception) -> None:
        self.failures += 1
        print(f"call failed: {error!r}", file=sys.stderr)
        if self.failures >= MAX_CONSECUTIVE_FAILURES:
            raise QuestionsError(f"{self.failures} calls failed in a row; stopping")

    def add(self, topic: str, requests: list[str]) -> None:
        fresh: list[Question] = []
        for text in requests:
            key = dedupe_key(text)
            if key in self.seen or self.count + len(fresh) >= self.settings.target:
                continue
            self.seen.add(key)
            fresh.append(Question(text=text, topic=topic))
        self.count += len(fresh)
        with self.out.open("a", encoding="utf-8") as file:
            file.writelines(json.dumps({"text": q.text, "topic": q.topic}) + "\n" for q in fresh)
        print(
            f"{self.count}/{self.settings.target} requests, {self.tokens} tokens", file=sys.stderr
        )


async def generate(
    settings: Settings,
    out: Path,
    *,
    transport: httpx.AsyncBaseTransport | None = None,
    sleep: Sleep = asyncio.sleep,
) -> tuple[int, int]:
    """Appends requests to `out` until the target or token budget is reached.

    Returns the total number of requests in `out` and the tokens this run used. The
    budget is checked between calls, so in-flight calls can overshoot it slightly.
    """
    out.parent.mkdir(parents=True, exist_ok=True)
    async with httpx.AsyncClient(transport=transport, timeout=CALL_TIMEOUT_SECONDS) as client:
        run = _Run(settings, client, out, sleep)
        try:
            async with asyncio.TaskGroup() as group:
                for _ in range(settings.concurrency):
                    group.create_task(run.worker())
        except* QuestionsError as errors:
            raise errors.exceptions[0] from None
    return run.count, run.tokens


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path, help="questions JSONL; existing rows are kept")
    parser.add_argument("--count", type=int, default=6000, help="target number of requests")
    parser.add_argument("--topic", action="append", choices=sorted(TOPICS), help="limit topics")
    parser.add_argument("--batch-size", type=int, default=25, help="requests asked per call")
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--max-total-tokens", type=int, default=3_000_000)
    args = parser.parse_args(argv)
    api_key = os.environ.get("BASETEN_API_KEY", "")
    if not api_key:
        parser.error("BASETEN_API_KEY is not set")
    settings = Settings(
        api_key=api_key,
        target=args.count,
        batch_size=args.batch_size,
        concurrency=args.concurrency,
        max_total_tokens=args.max_total_tokens,
        topics=tuple(args.topic) if args.topic else tuple(TOPICS),
    )
    try:
        total, tokens = asyncio.run(generate(settings, args.out))
    except QuestionsError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"{total} requests in {args.out}; this run used {tokens} tokens")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
