import json
import os
from pathlib import Path

import pytest
import torch
from tiny_glimmer import FakeTokenizer
from tiny_whisper import write_audio_manifest
from transformers import AutoTokenizer

from thalvi_train.batch import make_batch
from thalvi_train.glimmer import AUDIO_TOKEN_ID, PAD_TOKEN_ID
from thalvi_train.manifest import ManifestError
from thalvi_train.prompts import (
    AUDIO_TOKEN_TEXT,
    SYSTEM_PROMPT,
    GlimmerTokenizer,
    build_example,
    generation_ids,
    user_turn,
)
from thalvi_train.records import Record, read_records, resolve_audio
from thalvi_train.teacher import EOT_ID

RESPONSE = (
    "<|start|>assistant to=self<|message|>think<|eom|>"
    "<|start|>assistant to=user<|message|>ok<|eot|>"
)


def record(id: str = "a", task: str = "answer", split: str = "train", response: str = RESPONSE):
    return Record(id=id, text="hello there", task=task, split=split, response=response)


def test_reads_one_split_up_to_the_limit_and_ignores_other_fields(tmp_path: Path) -> None:
    rows = [
        {"id": "a", "text": "t", "task": "answer", "split": "train", "response": RESPONSE, "x": 1},
        {"id": "b", "text": "t", "task": "continue", "split": "dev", "response": RESPONSE},
        {"id": "c", "text": "t", "task": "continue", "split": "train", "response": RESPONSE},
        {"id": "d", "text": "t", "task": "answer", "split": "train", "response": RESPONSE},
    ]
    manifest = tmp_path / "manifest.jsonl"
    manifest.write_text("\n".join(json.dumps(row) for row in rows))
    assert [r.id for r in read_records(manifest, "train", limit=2)] == ["a", "c"]


@pytest.mark.parametrize(
    ("task", "response"),
    [
        ("translate", RESPONSE),
        ("answer", "<|start|>assistant to=self<|message|>cut off"),
        ("answer", "no turn markers<|eot|>"),
    ],
)
def test_rejects_malformed_records(task: str, response: str) -> None:
    with pytest.raises(ValueError, match="a:"):
        record(task=task, response=response)


def test_audio_is_found_by_record_id_across_manifests_in_record_order(tmp_path: Path) -> None:
    first = write_audio_manifest(tmp_path / "one", {"a": 100, "b": 100})
    second = write_audio_manifest(tmp_path / "two", {"c": 100})
    records = [record("c"), record("a")]
    assert resolve_audio(records, [first, second]) == [
        tmp_path / "two" / "audio" / "c.wav",
        tmp_path / "one" / "audio" / "a.wav",
    ]


def test_a_record_without_audio_is_an_error_naming_its_id(tmp_path: Path) -> None:
    manifest = write_audio_manifest(tmp_path, {"a": 100})
    with pytest.raises(ManifestError, match="lost"):
        resolve_audio([record("a"), record("lost")], [manifest])


def test_an_id_in_two_manifests_is_an_error(tmp_path: Path) -> None:
    first = write_audio_manifest(tmp_path / "one", {"a": 100})
    second = write_audio_manifest(tmp_path / "two", {"a": 100})
    with pytest.raises(ManifestError, match="'a'"):
        resolve_audio([record("a")], [first, second])


@pytest.mark.parametrize(
    ("task", "teacher_user", "student_user"),
    [
        ("answer", "hello there", AUDIO_TOKEN_TEXT * 3),
        (
            "continue",
            "Continue the following text: hello there",
            "Continue the following text: " + AUDIO_TOKEN_TEXT * 3,
        ),
    ],
)
def test_prompts_differ_only_in_the_user_turn_and_share_the_response(
    task: str, teacher_user: str, student_user: str
) -> None:
    tok = FakeTokenizer()
    example = build_example(record(task=task), tok, audio_tokens=3)
    response = tok.text_ids(RESPONSE)
    assert example.response_ids == response
    assert example.teacher_ids == tok.chat_ids(SYSTEM_PROMPT, teacher_user) + response
    assert example.student_ids == tok.chat_ids(SYSTEM_PROMPT, student_user) + response
    assert example.student_ids.count(AUDIO_TOKEN_ID) == 3


def test_response_cap_keeps_only_the_first_tokens_on_both_sides() -> None:
    tok = FakeTokenizer()
    example = build_example(record(), tok, audio_tokens=3, max_response_tokens=5)
    assert example.response_ids == tok.text_ids(RESPONSE)[:5]
    assert example.teacher_ids[-5:] == example.response_ids
    assert example.student_ids[-5:] == example.response_ids


def test_batch_positions_predict_each_response_token_in_both_prompts() -> None:
    records = [
        record("a"),
        record("b", task="continue", response=RESPONSE[:-7] + "longer reply<|eot|>"),
    ]
    frames = [torch.randn(9, 4), torch.randn(3, 4)]
    batch = make_batch(records, frames, FakeTokenizer(), lambda n: -(-n // 4))
    assert batch.audio_tokens == [3, 1]
    assert batch.frames.shape == (2, 9, 4)
    for side in (batch.teacher, batch.student):
        following = side.input_ids[side.rows, side.cols + 1]
        assert torch.equal(following, batch.targets)
        assert bool(side.attention_mask[side.rows, side.cols + 1].all())
        padding = side.attention_mask == 0
        assert bool((side.input_ids[padding] == PAD_TOKEN_ID).all())


GLIMMER_PATH = Path(os.environ.get("THALVI_GLIMMER_PATH", "~/models/Muse-Glimmer-30B")).expanduser()


@pytest.mark.skipif(
    not (GLIMMER_PATH / "tokenizer.json").exists(), reason="no local Glimmer tokenizer"
)
def test_real_tokenizer_renders_the_documented_template() -> None:
    tok = GlimmerTokenizer(AutoTokenizer.from_pretrained(GLIMMER_PATH))
    example = build_example(record(), tok, audio_tokens=2)
    expected = (
        "<|begin_of_text|><|start|>system<|message|>"
        + SYSTEM_PROMPT
        + '\n\nReasoning strength: low.\n\n# Valid recipients: "self", "user".<|eot|>'
        + "<|start|>user<|message|>"
        + AUDIO_TOKEN_TEXT * 2
        + "<|eot|>"
        + RESPONSE
    )
    assert example.student_ids == tok.text_ids(expected)
    assert example.student_ids.count(AUDIO_TOKEN_ID) == 2


@pytest.mark.skipif(
    not (GLIMMER_PATH / "tokenizer.json").exists(), reason="no local Glimmer tokenizer"
)
@pytest.mark.parametrize("task", ["answer", "continue"])
def test_teacher_prompt_ids_are_the_chat_template_with_generation_prompt(task: str) -> None:
    hf = AutoTokenizer.from_pretrained(GLIMMER_PATH)
    rendered = hf.apply_chat_template(
        [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_turn(task, "what time is it")},
        ],
        tokenize=False,
        add_generation_prompt=True,
        reasoning_strength="low",
    )
    assert rendered.endswith("<|start|>assistant")
    assert hf.convert_tokens_to_ids("<|eot|>") == EOT_ID
    assert generation_ids(task, "what time is it", GlimmerTokenizer(hf)) == hf.encode(
        rendered, add_special_tokens=False
    )
