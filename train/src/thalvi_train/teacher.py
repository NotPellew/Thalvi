"""Has Glimmer answer each transcript and writes the training manifest.

Run as `python -m thalvi_train.teacher`.

The teacher is Glimmer reading the transcript. Prompts go to a vLLM
`/v1/completions` endpoint as token ids built by `prompts.generation_ids`, the
same code training uses for the text side, so the server never retokenizes or
adds a BOS. Output rows are appended as answers arrive, which makes a rerun
resume: ids already in the output are skipped.
"""

import argparse
import asyncio
import json
import re
import sys
from collections.abc import Iterable
from dataclasses import asdict, dataclass
from pathlib import Path

import httpx
from transformers import AutoTokenizer

from thalvi_train.http_retry import Sleep, post_with_retry
from thalvi_train.manifest import ManifestError, Sample, read_manifest
from thalvi_train.prompts import (
    GENERATION_PROMPT,
    GlimmerTokenizer,
    PromptTokenizer,
    generation_ids,
)
from thalvi_train.records import RESPONSE_END, Record

EOT_ID = 200008
DEFAULT_URL = "http://127.0.0.1:8000"

_NO_SPECIAL_TOKEN = r"((?:(?!<\|).)*)"
_REASONING_AND_ANSWER = re.compile(
    "<\\|start\\|>assistant to=self<\\|message\\|>"
    + _NO_SPECIAL_TOKEN
    + "<\\|eom\\|><\\|start\\|>assistant to=user<\\|message\\|>"
    + _NO_SPECIAL_TOKEN
    + "<\\|eot\\|>",
    re.DOTALL,
)


@dataclass(frozen=True)
class Sampling:
    """How the teacher decodes. Greedy by default, since the answer is a distillation target."""

    max_tokens: int = 512
    temperature: float = 0.0
    top_p: float = 1.0


@dataclass
class Tally:
    """What happened to each sample this run. `failed` ones are retried by the next run."""

    written: int = 0
    unfinished: int = 0
    malformed: int = 0
    failed: int = 0


def parse_response(response: str) -> tuple[str, str] | None:
    """Splits a response into reasoning and answer, or `None` if it is not exactly that shape."""
    match = _REASONING_AND_ANSWER.fullmatch(response)
    if match is None or not match.group(2).strip():
        return None
    return match.group(1), match.group(2)


def finished_generation(choice: dict[str, object]) -> str | None:
    """The generated text if the server stopped on `<|eot|>`, else `None`.

    vLLM keeps a stop token's text only with `include_stop_str_in_output`; a
    server that strips it still reports the token id as `stop_reason`, so the
    text is completed here.
    """
    text = choice.get("text")
    if choice.get("finish_reason") != "stop" or not isinstance(text, str):
        return None
    if text.endswith(RESPONSE_END):
        return text
    if choice.get("stop_reason") == EOT_ID:
        return text + RESPONSE_END
    return None


def completion_request(model: str, prompt_ids: list[int], sampling: Sampling) -> dict[str, object]:
    """The `/v1/completions` body: raw ids in, special tokens out, stopping on `<|eot|>`."""
    return {
        "model": model,
        "prompt": prompt_ids,
        "max_tokens": sampling.max_tokens,
        "temperature": sampling.temperature,
        "top_p": sampling.top_p,
        "stop_token_ids": [EOT_ID],
        "include_stop_str_in_output": True,
        "skip_special_tokens": False,
    }


def read_done(output: Path) -> set[str]:
    """Ids already in `output`; a torn last line from an interrupted run is cut off."""
    if not output.exists():
        return set()
    lines = output.read_text(encoding="utf-8").splitlines(keepends=True)
    done: set[str] = set()
    kept = 0
    for line in lines:
        try:
            done.add(json.loads(line)["id"])
        except (json.JSONDecodeError, KeyError):
            break
        kept += 1
    if kept < len(lines) or (lines and not lines[-1].endswith("\n")):
        output.write_text("".join(f"{line.rstrip()}\n" for line in lines[:kept]), encoding="utf-8")
    return done


async def answer(
    samples: Iterable[Sample],
    output: Path,
    tokenizer: PromptTokenizer,
    *,
    model: str,
    base_url: str = DEFAULT_URL,
    sampling: Sampling,
    concurrency: int = 64,
    transport: httpx.AsyncBaseTransport | None = None,
    sleep: Sleep = asyncio.sleep,
) -> Tally:
    """Appends a `Record` to `output` for each sample the teacher answers well.

    Samples whose generation hit `max_tokens`, or is not reasoning plus an
    answer, are dropped and counted. Requests that fail after retries are
    counted in `failed` and left for the next run.
    """
    done = read_done(output)
    pending = [sample for sample in samples if sample.id not in done]
    tally = Tally()
    gate = asyncio.Semaphore(concurrency)
    output.parent.mkdir(parents=True, exist_ok=True)
    async with httpx.AsyncClient(transport=transport, timeout=600.0) as client:
        with output.open("a", encoding="utf-8") as sink:

            async def one(sample: Sample) -> None:
                body = completion_request(
                    model, generation_ids(sample.task, sample.text, tokenizer), sampling
                )
                async with gate:
                    try:
                        reply = await post_with_retry(
                            client, f"{base_url}/v1/completions", json=body, sleep=sleep
                        )
                        reply.raise_for_status()
                        choice = reply.json()["choices"][0]
                    except (httpx.HTTPError, KeyError, IndexError, ValueError) as error:
                        print(f"{sample.id} failed: {error!r}", file=sys.stderr)
                        tally.failed += 1
                        return
                generated = finished_generation(choice)
                if generated is None:
                    tally.unfinished += 1
                    return
                response = GENERATION_PROMPT + generated
                if parse_response(response) is None:
                    tally.malformed += 1
                    return
                record = Record(
                    id=sample.id,
                    text=sample.text,
                    task=sample.task,
                    split=sample.split,
                    response=response,
                )
                sink.write(json.dumps(asdict(record), ensure_ascii=False) + "\n")
                sink.flush()
                tally.written += 1

            await asyncio.gather(*(one(sample) for sample in pending))
    return tally


def read_manifests(paths: list[Path]) -> list[Sample]:
    """All samples of `paths` in order; an id repeated across manifests is an error."""
    samples: list[Sample] = []
    seen: set[str] = set()
    for path in paths:
        for sample in read_manifest(path):
            if sample.id in seen:
                raise ManifestError(f"{path}: id {sample.id!r} is already in an earlier manifest")
            seen.add(sample.id)
            samples.append(sample)
    return samples


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("manifests", type=Path, nargs="+", help="data manifests to answer")
    parser.add_argument("--out", type=Path, required=True, help="training manifest JSONL to append")
    parser.add_argument("--glimmer", type=Path, required=True, help="directory with the tokenizer")
    parser.add_argument("--model", default="glimmer", help="model name the server serves")
    parser.add_argument("--base-url", default=DEFAULT_URL, help="vLLM server address")
    parser.add_argument("--concurrency", type=int, default=64)
    parser.add_argument("--max-tokens", type=int, default=Sampling.max_tokens)
    parser.add_argument("--temperature", type=float, default=Sampling.temperature)
    parser.add_argument("--top-p", type=float, default=Sampling.top_p)
    parser.add_argument("--limit", type=int, help="answer only the first N samples")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    try:
        samples = read_manifests(args.manifests)[: args.limit]
    except ManifestError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    tokenizer = GlimmerTokenizer(AutoTokenizer.from_pretrained(args.glimmer))
    tally = asyncio.run(
        answer(
            samples,
            args.out,
            tokenizer,
            model=args.model,
            base_url=args.base_url,
            sampling=Sampling(args.max_tokens, args.temperature, args.top_p),
            concurrency=args.concurrency,
        )
    )
    print(
        f"{tally.written} written to {args.out}; {tally.unfinished} hit max tokens, "
        f"{tally.malformed} malformed, {tally.failed} failed"
    )
    return 1 if tally.failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
