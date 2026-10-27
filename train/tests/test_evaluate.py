import json
from pathlib import Path

import pytest
import torch
from safetensors.torch import save_file
from tiny_glimmer import HIDDEN, FakeTokenizer

from thalvi_train import evaluate
from thalvi_train.batch import make_batch
from thalvi_train.evaluate import (
    Baseline,
    SampleResult,
    edit_distance,
    evaluate_record,
    generate,
    normalize,
    split_reply,
    summarize,
    transcript_of,
)
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.projector import AudioProjector
from thalvi_train.records import Record
from thalvi_train.train import PROJECTOR_CONFIG, PROJECTOR_WEIGHTS, ProjectorConfig, distill_loss

PROJECTOR = ProjectorConfig(encoder_dim=16, hidden_dim=32, text_dim=HIDDEN, stack=4)
RESPONSE = (
    "<|start|>assistant to=self<|message|>hmm<|eom|><|start|>assistant to=user<|message|>yes<|eot|>"
)
RECORDS = [
    Record("a", "what is two plus two", "answer", "eval", RESPONSE),
    Record("b", "once upon a time", "continue", "eval", RESPONSE),
    Record("c", "ignored train row", "answer", "train", RESPONSE),
]


@pytest.fixture
def glimmer(tiny_glimmer_dir: Path) -> FrozenGlimmer:
    return load_glimmer(tiny_glimmer_dir, dtype=torch.float32)


@pytest.fixture
def features(tmp_path: Path) -> Path:
    torch.manual_seed(1)
    directory = tmp_path / "features"
    directory.mkdir()
    for name, frames in (("a", 100), ("b", 5), ("c", 10)):
        save_file({"frames": torch.randn(frames, 16).half()}, directory / f"{name}.safetensors")
    return directory


@pytest.fixture
def projector() -> AudioProjector:
    torch.manual_seed(2)
    return AudioProjector(**PROJECTOR.__dict__).eval()


@pytest.mark.parametrize(
    ("reference", "hypothesis", "distance"),
    [
        ("", "", 0),
        ("a b c", "a b c", 0),
        ("a b c", "", 3),
        ("", "a b", 2),
        ("a b c", "a x c", 1),
        ("a b c", "a c", 1),
        ("a c", "a b c", 1),
        ("kitten sitting", "sitting kitten", 2),
        ("a b c d", "d c b a", 4),
        ("one two three four", "one three four five six", 3),
    ],
)
def test_edit_distance_counts_word_substitutions_insertions_and_deletions(
    reference: str, hypothesis: str, distance: int
) -> None:
    assert edit_distance(reference.split(), hypothesis.split()) == distance


@pytest.mark.parametrize(
    ("text", "words"),
    [
        ("No wait, take off APPLES!", ["no", "wait", "take", "off", "apples"]),
        ("  two   spaces\tand\nlines ", ["two", "spaces", "and", "lines"]),
        ("don't won\N{RIGHT SINGLE QUOTATION MARK}t", ["dont", "wont"]),
        ("well-known \N{EM DASH} really", ["well", "known", "really"]),
        ("3 apples, 4.5 pears", ["3", "apples", "45", "pears"]),
        ("...", []),
    ],
)
def test_normalize_lowercases_and_drops_punctuation(text: str, words: list[str]) -> None:
    assert normalize(text) == words


@pytest.mark.parametrize(
    ("reasoning", "task", "expected"),
    [
        ("Add green apples.\n\nWe need a short reply.", "answer", "Add green apples."),
        ("Add green apples.\n  \nMore", "answer", "Add green apples."),
        (
            "Add green apples. We need a short reply.",
            "answer",
            "Add green apples. We need a short reply.",
        ),
        ("\n\n  Add green apples.\n\nMore\n\nEven more", "answer", "Add green apples."),
        ("", "answer", ""),
        ("  \n\n ", "answer", ""),
        ("Continue the following text: once upon\n\nplan", "continue", "once upon"),
        ("once upon\n\nplan", "continue", "once upon"),
        (
            "Continue the following text: once upon",
            "answer",
            "Continue the following text: once upon",
        ),
    ],
)
def test_transcript_is_the_first_paragraph_of_the_reasoning(
    reasoning: str, task: str, expected: str
) -> None:
    assert transcript_of(reasoning, task) == expected


@pytest.mark.parametrize(
    ("generated", "reasoning", "answer"),
    [
        (
            " to=self<|message|>think<|eom|><|start|>assistant to=user<|message|>reply<|eot|>",
            "think",
            "reply",
        ),
        (" to=self<|message|>cut off mid", "cut off mid", ""),
        (" to=self<|message|>think<|eom|><|start|>assistant to=user<|message|>par", "think", "par"),
        (" to=self<|message|>think<|eot|>", "think", ""),
        (" to=user<|message|>no reasoning<|eot|>", "", "no reasoning"),
        ("<123><456>", "", ""),
    ],
)
def test_split_reply_reads_whatever_part_of_the_reply_was_generated(
    generated: str, reasoning: str, answer: str
) -> None:
    assert split_reply(generated) == (reasoning, answer)


def uncached_greedy(glimmer: FrozenGlimmer, prompt: torch.Tensor, steps: int) -> list[int]:
    """Greedy ids recomputing the whole sequence at every step, with no cache."""
    embeds, ids = prompt, []
    for _ in range(steps):
        mask = torch.ones(embeds.shape[:2], dtype=torch.long)
        token = int(glimmer.logits(glimmer.hidden_states(embeds, mask)[:, -1]).argmax())
        ids.append(token)
        embeds = torch.cat([embeds, glimmer.embed_text(torch.tensor([[token]]))], dim=1)
    return ids


def test_cached_greedy_generation_matches_recomputing_the_full_sequence(
    glimmer: FrozenGlimmer,
) -> None:
    torch.manual_seed(3)
    prompt = torch.randn(1, 13, HIDDEN)
    steps = 20
    expected = uncached_greedy(glimmer, prompt, steps)
    generation = generate(glimmer, prompt, steps, stop_id=-1)
    assert generation.ids == expected
    assert len(set(expected)) > 3
    assert not generation.finished
    assert 0 < generation.time_to_first_token_s <= generation.seconds


def test_generation_stops_after_the_stop_id_and_not_past_the_limit(glimmer: FrozenGlimmer) -> None:
    torch.manual_seed(4)
    prompt = torch.randn(1, 6, HIDDEN)
    free = generate(glimmer, prompt, 10, stop_id=-1).ids
    stop = next(token for token in free[1:] if token not in free[:1])
    cut = free.index(stop)

    stopped = generate(glimmer, prompt, 10, stop_id=stop)
    assert (stopped.ids, stopped.finished) == (free[: cut + 1], True)
    limited = generate(glimmer, prompt, cut, stop_id=stop)
    assert (limited.ids, limited.finished) == (free[:cut], False)
    with pytest.raises(ValueError, match="max_new_tokens"):
        generate(glimmer, prompt, 0)


def test_a_record_reports_its_training_kl_audio_length_and_optional_baseline(
    glimmer: FrozenGlimmer, projector: AudioProjector, features: Path
) -> None:
    frames = evaluate.load_frames(features, RECORDS[0], PROJECTOR.encoder_dim)
    kwargs = {"max_new_tokens": 6}
    plain = evaluate_record(
        glimmer, FakeTokenizer(), projector, RECORDS[0], frames, with_text_baseline=False, **kwargs
    )
    compared = evaluate_record(
        glimmer, FakeTokenizer(), projector, RECORDS[0], frames, with_text_baseline=True, **kwargs
    )

    batch = make_batch(RECORDS[:1], [frames], FakeTokenizer(), projector.audio_token_count)
    with torch.no_grad():
        expected_kl = distill_loss(glimmer, projector, batch).kl.item()
    assert plain.kl == pytest.approx(expected_kl, rel=1e-5)
    assert plain.audio_seconds == 2.0
    assert plain.new_tokens == 6
    assert plain.baseline is None
    assert compared.response == plain.response
    assert isinstance(compared.baseline, Baseline)
    assert compared.baseline.response.startswith("<|start|>assistant")
    assert plain.reference_words == 5
    assert plain.wer == plain.edits / 5


def result(task: str, edits: int, words: int, **overrides) -> SampleResult:
    fields = {
        "id": "x",
        "task": task,
        "text": "",
        "kl": 1.0,
        "transcript": "",
        "edits": edits,
        "reference_words": words,
        "wer": edits / words,
        "answer": "",
        "finished": True,
        "new_tokens": 1,
        "audio_seconds": 1.0,
        "time_to_first_token_s": 0.1,
        "generate_s": 1.0,
        "response": "",
        "baseline": None,
    }
    return SampleResult(**(fields | overrides))


def test_summary_pools_edits_over_words_instead_of_averaging_sample_wers() -> None:
    rows = [
        result("answer", 1, 2, kl=1.0, time_to_first_token_s=0.3),
        result("answer", 0, 8, kl=3.0, finished=False, time_to_first_token_s=0.1),
        result("continue", 3, 10, kl=5.0, time_to_first_token_s=0.2),
    ]
    summary = summarize(rows)
    assert summary["samples"] == 3
    assert summary["wer"] == pytest.approx(4 / 20)
    assert summary["mean_kl"] == pytest.approx(3.0)
    assert summary["finished_rate"] == pytest.approx(2 / 3)
    assert summary["median_time_to_first_token_s"] == pytest.approx(0.2)
    assert summary["by_task"]["answer"] == {
        "samples": 2,
        "mean_kl": 2.0,
        "wer": pytest.approx(1 / 10),
        "finished_rate": 0.5,
    }
    assert summary["by_task"]["continue"]["wer"] == pytest.approx(0.3)
    assert "baseline_finished_rate" not in summary


def test_summary_adds_the_baseline_finish_rate_only_when_every_sample_has_one() -> None:
    done = Baseline("", "", True, 1.0)
    cut = Baseline("", "", False, 1.0)
    both = summarize([result("answer", 0, 1, baseline=done), result("answer", 0, 1, baseline=cut)])
    assert both["baseline_finished_rate"] == 0.5
    mixed = summarize([result("answer", 0, 1, baseline=done), result("answer", 0, 1)])
    assert "baseline_finished_rate" not in mixed


def test_cli_writes_a_row_per_eval_sample_and_a_summary(
    tiny_glimmer_dir: Path,
    projector: AudioProjector,
    features: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    manifest = tmp_path / "teacher.jsonl"
    manifest.write_text(
        "".join(json.dumps(r.__dict__ | {"source": "x"}) + "\n" for r in RECORDS),
        encoding="utf-8",
    )
    run = tmp_path / "run"
    run.mkdir()
    (run / PROJECTOR_CONFIG).write_text(json.dumps(PROJECTOR.__dict__))
    save_file(projector.state_dict(), run / PROJECTOR_WEIGHTS)
    monkeypatch.setattr(evaluate, "_load_tokenizer", lambda path: FakeTokenizer())
    out = run / "eval.jsonl"

    evaluate.main(
        [
            *("--manifest", str(manifest)),
            *("--features-dir", str(features)),
            *("--glimmer", str(tiny_glimmer_dir)),
            *("--projector", str(run)),
            *("--out", str(out)),
            *("--max-new-tokens", "5"),
            "--with-text-baseline",
        ]
    )

    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert [(row["id"], row["task"]) for row in rows] == [("a", "answer"), ("b", "continue")]
    assert all(row["new_tokens"] <= 5 and row["kl"] > 0 for row in rows)
    assert all(row["baseline"]["response"] for row in rows)
    summary = json.loads((run / "eval.summary.json").read_text(encoding="utf-8"))
    assert summary["samples"] == 2
    assert set(summary["by_task"]) == {"answer", "continue"}
    assert summary["mean_kl"] == pytest.approx(sum(row["kl"] for row in rows) / 2)


def test_cli_refuses_a_projector_trained_for_another_hidden_size(
    tiny_glimmer_dir: Path, tmp_path: Path
) -> None:
    run = tmp_path / "run"
    run.mkdir()
    (run / PROJECTOR_CONFIG).write_text(json.dumps(PROJECTOR.__dict__ | {"text_dim": HIDDEN + 1}))
    with pytest.raises(ValueError, match="text_dim"):
        evaluate.load_projector(run, HIDDEN, torch.device("cpu"))
