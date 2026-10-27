import os
from pathlib import Path

import pytest
from tiny_glimmer import FakeTokenizer
from transformers import AutoTokenizer

from thalvi_train.glimmer import AUDIO_TOKEN_ID
from thalvi_train.history import (
    MAX_ANSWER_CHARS,
    HistoryConfig,
    HistorySampler,
    answer_of,
    history_chars,
)
from thalvi_train.prompts import (
    AUDIO_TOKEN_TEXT,
    SYSTEM_PROMPT,
    GlimmerTokenizer,
    build_example,
    generation_ids,
)
from thalvi_train.records import Record

HISTORY = (("what is the capital of france", "Paris."), ("tell me a joke", "Why did the hen."))


def reply(answer: str, reasoning: str = "The user wants an answer.") -> str:
    return (
        f"<|start|>assistant to=self<|message|>{reasoning}<|eom|>"
        f"<|start|>assistant to=user<|message|>{answer}<|eot|>"
    )


def corpus(size: int = 40) -> list[Record]:
    return [
        Record(
            f"r{i}",
            f"question number {i}",
            "answer" if i % 2 else "continue",
            "train",
            reply(f"answer number {i}"),
        )
        for i in range(size)
    ]


def sampler(records: list[Record], seed: int = 0, **config) -> HistorySampler:
    return HistorySampler(records, HistoryConfig(**({"prob": 1.0} | config)), seed)


def test_teacher_and_student_prompts_share_the_history_and_differ_in_the_current_turn() -> None:
    tok = FakeTokenizer()
    record = Record("a", "hello there", "answer", "train", reply("ok"))
    example = build_example(record, tok, audio_tokens=3, history=HISTORY)
    response = tok.text_ids(record.response)

    assert example.teacher_ids == tok.chat_ids(SYSTEM_PROMPT, "hello there", HISTORY) + response
    assert example.student_ids == (
        tok.chat_ids(SYSTEM_PROMPT, AUDIO_TOKEN_TEXT * 3, HISTORY) + response
    )
    shared = tok.chat_ids(SYSTEM_PROMPT, "", HISTORY)[:-1]
    assert example.teacher_ids[: len(shared)] == example.student_ids[: len(shared)] == shared
    assert example.student_ids.count(AUDIO_TOKEN_ID) == 3


def test_history_comes_before_the_current_turn_in_the_generation_prompt() -> None:
    tok = FakeTokenizer()
    plain = generation_ids("answer", "now", tok)
    with_history = generation_ids("answer", "now", tok, HISTORY)
    current = plain[len(tok.chat_ids(SYSTEM_PROMPT, "")) - 1 :]
    assert with_history[-len(current) :] == current
    assert with_history == (
        tok.chat_ids(SYSTEM_PROMPT, "now", HISTORY) + tok.text_ids("<|start|>assistant")
    )
    assert len(with_history) == len(plain) + history_chars(HISTORY) + 4 * len(HISTORY)


def test_a_history_turn_holds_the_answer_without_the_reasoning() -> None:
    records = [
        Record("a", "first", "answer", "train", reply("The answer.", reasoning="secret thoughts")),
        Record("b", "second", "answer", "train", reply("Other.", reasoning="more thoughts")),
    ]
    history = sampler(records, turns=1).draw(0)
    assert history == (("second", "Other."),)
    assert answer_of(records[0].response) == "The answer."
    assert answer_of("<|start|>assistant to=self<|message|>only thinking") == ""
    assert answer_of("<|start|>assistant to=user<|message|>cut off") == "cut off"


def test_a_long_answer_is_cut_and_a_record_without_one_is_never_drawn() -> None:
    records = [
        Record("a", "first", "answer", "train", reply("x" * 1000)),
        Record("b", "second", "answer", "train", reply("fine")),
        Record("c", "third", "answer", "train", "<|start|>assistant to=self<|message|>t<|eot|>"),
    ]
    drawn = sampler(records, same_task=0.0, turns=2)
    assert drawn.draw(1) == (("first", "x" * MAX_ANSWER_CHARS),)
    assert all(
        text != "third" for index in range(3) for text, _ in sampler(records, turns=2).draw(index)
    )
    assert drawn.draw(2) != ()


def test_drawing_is_reproducible_and_never_picks_the_current_record() -> None:
    records = corpus()
    first = sampler(records).draw_all()
    assert first == sampler(corpus()).draw_all()
    assert first != sampler(records, seed=1).draw_all()
    assert sampler(records).draw_all(epoch=1) != first
    for index, history in enumerate(first):
        assert 1 <= len(history) <= 3
        assert records[index].text not in [text for text, _ in history]
        assert len({text for text, _ in history}) == len(history)
    assert sampler(records).draw(7) == first[7]


def test_history_is_drawn_for_a_share_of_samples_set_by_the_probability() -> None:
    records = corpus(400)
    drawn = sum(bool(h) for h in sampler(records, prob=0.5).draw_all())
    assert 150 < drawn < 250
    assert not any(sampler(records, prob=0.0).draw_all())
    assert all(sampler(records, prob=1.0).draw_all())


def test_same_task_history_comes_from_records_of_the_same_task() -> None:
    records = corpus()
    tasks = {record.text: record.task for record in records}
    for index, history in enumerate(sampler(records, same_task=1.0).draw_all()):
        assert {tasks[text] for text, _ in history} == {records[index].task}
    mixed = {
        tasks[text] != records[index].task
        for index, history in enumerate(sampler(records, same_task=0.0).draw_all())
        for text, _ in history
    }
    assert mixed == {True, False}


def test_a_lone_record_or_task_falls_back_to_what_there_is() -> None:
    assert sampler(corpus(1)).draw(0) == ()
    records = [
        Record("a", "alone", "continue", "train", reply("one")),
        Record("b", "first", "answer", "train", reply("two")),
        Record("c", "second", "answer", "train", reply("three")),
    ]
    assert {text for text, _ in sampler(records, same_task=1.0).draw(0)} <= {"first", "second"}
    assert sampler(records, same_task=1.0).draw(0) != ()


@pytest.mark.parametrize("bad", [{"prob": 1.5}, {"prob": -0.1}, {"same_task": 2.0}, {"turns": 0}])
def test_rejects_out_of_range_history_settings(bad: dict) -> None:
    with pytest.raises(ValueError, match="history"):
        HistoryConfig(**bad)


GLIMMER_TEXT_PATH = Path(
    os.environ.get("THALVI_GLIMMER_TEXT_PATH", "~/models/Muse-Glimmer-30B-text")
).expanduser()


@pytest.mark.skipif(
    not (GLIMMER_TEXT_PATH / "tokenizer.json").exists(), reason="no local Glimmer tokenizer"
)
def test_real_tokenizer_renders_history_as_earlier_turns_before_the_audio_turn() -> None:
    tok = GlimmerTokenizer(AutoTokenizer.from_pretrained(GLIMMER_TEXT_PATH))
    record = Record("a", "hello there", "answer", "train", reply("ok", reasoning="think"))
    example = build_example(record, tok, audio_tokens=2, history=HISTORY)
    expected = (
        "<|begin_of_text|><|start|>system<|message|>"
        + SYSTEM_PROMPT
        + '\n\nReasoning strength: low.\n\n# Valid recipients: "self", "user".<|eot|>'
        + "<|start|>user<|message|>what is the capital of france<|eot|>"
        + "<|start|>assistant to=user<|message|>Paris.<|eot|>"
        + "<|start|>user<|message|>tell me a joke<|eot|>"
        + "<|start|>assistant to=user<|message|>Why did the hen.<|eot|>"
        + "<|start|>user<|message|>"
        + AUDIO_TOKEN_TEXT * 2
        + "<|eot|>"
        + record.response
    )
    assert example.student_ids == tok.text_ids(expected)
    assert example.teacher_ids == tok.text_ids(
        expected.replace(AUDIO_TOKEN_TEXT * 2, "hello there")
    )
