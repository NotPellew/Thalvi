import asyncio
import json
from pathlib import Path

import httpx
import pytest
from tiny_glimmer import FakeTokenizer

from thalvi_train.manifest import ManifestError, Sample, write_manifest
from thalvi_train.prompts import generation_ids
from thalvi_train.records import read_records
from thalvi_train.teacher import (
    EOT_ID,
    Sampling,
    Tally,
    answer,
    parse_response,
    read_manifests,
)

ANSWER = (
    "<|start|>assistant to=self<|message|>The user wants the time.<|eom|>"
    "<|start|>assistant to=user<|message|>It is noon.<|eot|>"
)
GENERATED = ANSWER.removeprefix("<|start|>assistant")
TOKENIZER = FakeTokenizer()
BASE = "http://vllm.test"


def sample(id: str, task: str = "answer") -> Sample:
    return Sample(
        id=id,
        audio=f"{id}.wav",
        text=f"what is {id}",
        task=task,  # type: ignore[arg-type]
        split="train",
        voice="af_heart",
        source="kokoro",
    )


def choice(text: str, finish_reason: str = "stop", stop_reason: int | None = EOT_ID) -> dict:
    return {"text": text, "finish_reason": finish_reason, "stop_reason": stop_reason}


def serve(replies: dict[str, httpx.Response], requests: list[dict] | None = None):
    by_prompt = {
        tuple(generation_ids(s.task, s.text, TOKENIZER)): replies[s.id]
        for s in map(sample, replies)
    }

    def respond(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        if requests is not None:
            requests.append(body)
        return by_prompt[tuple(body["prompt"])]

    return httpx.MockTransport(respond)


def ok(text: str, **fields: object) -> httpx.Response:
    return httpx.Response(200, json={"choices": [{**choice(text), **fields}]})


def run(samples: list[Sample], out: Path, transport: httpx.MockTransport, **kwargs) -> Tally:
    async def no_sleep(_seconds: float) -> None:
        return None

    return asyncio.run(
        answer(
            samples,
            out,
            TOKENIZER,
            model="glimmer",
            base_url=BASE,
            sampling=kwargs.pop("sampling", Sampling()),
            transport=transport,
            sleep=no_sleep,
            **kwargs,
        )
    )


@pytest.mark.parametrize(
    ("response", "parsed"),
    [
        (ANSWER, ("The user wants the time.", "It is noon.")),
        (ANSWER.replace("The user wants the time.", ""), ("", "It is noon.")),
        (ANSWER.replace("It is noon.", "  "), None),
        (ANSWER.replace("<|eom|><|start|>assistant to=user", "<|eot|>"), None),
        (ANSWER.replace("It is noon.", "It is <|start|>noon."), None),
        (ANSWER + "<|start|>assistant to=user<|message|>again<|eot|>", None),
    ],
)
def test_response_must_be_reasoning_then_one_answer(
    response: str, parsed: tuple[str, str] | None
) -> None:
    assert parse_response(response) == parsed


def test_requests_carry_the_teacher_prompt_ids_and_stop_on_eot(tmp_path: Path) -> None:
    requests: list[dict] = []
    samples = [sample("a"), sample("b", task="continue")]
    transport = serve({"a": ok(GENERATED), "b": ok(GENERATED)}, requests)
    run(samples, tmp_path / "out.jsonl", transport, sampling=Sampling(256, 0.7, 0.95))
    assert sorted(r["prompt"] for r in requests) == sorted(
        generation_ids(s.task, s.text, TOKENIZER) for s in samples
    )
    assert all(
        r
        == {
            "model": "glimmer",
            "prompt": r["prompt"],
            "max_tokens": 256,
            "temperature": 0.7,
            "top_p": 0.95,
            "stop_token_ids": [EOT_ID],
            "include_stop_str_in_output": True,
            "skip_special_tokens": False,
        }
        for r in requests
    )


def test_each_sample_is_written_dropped_or_failed_and_counted(tmp_path: Path) -> None:
    out = tmp_path / "out.jsonl"
    transport = serve(
        {
            "good": ok(GENERATED),
            "stripped": ok(GENERATED.removesuffix("<|eot|>")),
            "cut": ok(GENERATED[:-20], finish_reason="length", stop_reason=None),
            "other_eos": ok(
                GENERATED.removesuffix("<|eot|>") + "<|end_of_text|>", stop_reason=None
            ),
            "malformed": ok("<|message|>no channel<|eot|>"),
            "down": httpx.Response(400, json={"error": "bad"}),
            "no_choices": httpx.Response(200, json={"choices": []}),
        }
    )
    samples = [sample(id) for id in ("good", "stripped", "cut", "other_eos", "malformed", "down")]
    tally = run([*samples, sample("no_choices")], out, transport)
    assert tally == Tally(written=2, unfinished=2, malformed=1, failed=2)
    records = read_records(out, "train")
    assert sorted(r.id for r in records) == ["good", "stripped"]
    assert {r.response for r in records} == {"<|start|>assistant" + GENERATED}
    assert {r.text for r in records} == {"what is good", "what is stripped"}


def test_a_rerun_skips_written_ids_and_repairs_a_torn_last_line(tmp_path: Path) -> None:
    out = tmp_path / "out.jsonl"
    first = run([sample("a")], out, serve({"a": ok(GENERATED)}))
    assert first.written == 1
    with out.open("a") as file:
        file.write('{"id": "torn", "te')
    requests: list[dict] = []
    second = run(
        [sample("a"), sample("b")], out, serve({"a": ok(GENERATED), "b": ok(GENERATED)}, requests)
    )
    assert second.written == 1
    assert len(requests) == 1
    assert sorted(r.id for r in read_records(out, "train")) == ["a", "b"]


def test_manifests_are_joined_and_a_repeated_id_is_an_error(tmp_path: Path) -> None:
    write_manifest(tmp_path / "one.jsonl", [sample("a"), sample("b")])
    write_manifest(tmp_path / "two.jsonl", [sample("c")])
    write_manifest(tmp_path / "clash.jsonl", [sample("b")])
    joined = read_manifests([tmp_path / "one.jsonl", tmp_path / "two.jsonl"])
    assert [s.id for s in joined] == ["a", "b", "c"]
    with pytest.raises(ManifestError, match="'b' is already"):
        read_manifests([tmp_path / "one.jsonl", tmp_path / "clash.jsonl"])
