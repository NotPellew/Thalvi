import json
from pathlib import Path

import pytest
import torch
import torch.nn.functional as F
from safetensors.torch import save_file
from tiny_glimmer import HIDDEN, FakeTokenizer
from tiny_noise import write_noise_manifest
from tiny_whisper import DIM, WINDOW, tone, write_audio_manifest

from thalvi_train import evaluate
from thalvi_train.batch import make_batch
from thalvi_train.echo import PREFILL
from thalvi_train.evaluate import (
    Baseline,
    SampleResult,
    edit_distance,
    evaluate_batch,
    generate,
    normalize,
    split_reply,
    summarize,
    transcript_of,
)
from thalvi_train.features import encode
from thalvi_train.glimmer import FrozenGlimmer, load_glimmer
from thalvi_train.projector import AudioProjector
from thalvi_train.prompts import GENERATION_PROMPT
from thalvi_train.records import Record, resolve_audio
from thalvi_train.train import PROJECTOR_CONFIG, PROJECTOR_WEIGHTS, ProjectorConfig, distill_loss

PROJECTOR = ProjectorConfig(encoder_dim=DIM, hidden_dim=32, text_dim=HIDDEN, stack=4)
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
def audio_manifest(tmp_path: Path) -> Path:
    return write_audio_manifest(tmp_path / "speech", {"a": WINDOW, "b": 1600, "c": 3200})


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


def solo(glimmer: FrozenGlimmer, prompt: torch.Tensor, steps: int, stop_id: int = -1):
    """`generate` on one unpadded prompt `(length, hidden)`."""
    return generate(
        glimmer, prompt[None], torch.ones(1, len(prompt), dtype=torch.long), steps, stop_id
    )


def test_cached_greedy_generation_matches_recomputing_the_full_sequence(
    glimmer: FrozenGlimmer,
) -> None:
    torch.manual_seed(3)
    prompt = torch.randn(13, HIDDEN)
    steps = 20
    expected = uncached_greedy(glimmer, prompt[None], steps)
    generation = solo(glimmer, prompt, steps)
    assert generation.ids == [expected]
    assert len(set(expected)) > 3
    assert generation.finished == [False]
    assert 0 < generation.prefill_s <= generation.seconds


def test_generation_stops_after_the_stop_id_and_not_past_the_limit(glimmer: FrozenGlimmer) -> None:
    torch.manual_seed(4)
    prompt = torch.randn(6, HIDDEN)
    (free,) = solo(glimmer, prompt, 10).ids
    stop = next(token for token in free[1:] if token not in free[:1])
    cut = free.index(stop)

    stopped = solo(glimmer, prompt, 10, stop)
    assert (stopped.ids, stopped.finished) == ([free[: cut + 1]], [True])
    limited = solo(glimmer, prompt, cut, stop)
    assert (limited.ids, limited.finished) == ([free[:cut]], [False])
    with pytest.raises(ValueError, match="max_new_tokens"):
        solo(glimmer, prompt, 0)


def test_left_padded_batch_generates_what_each_prompt_generates_alone(
    glimmer: FrozenGlimmer,
) -> None:
    """Prompts of 5, 13 and 9 positions and 24 steps run past the 8-position sliding window."""
    torch.manual_seed(5)
    prompts = [torch.randn(n, HIDDEN) for n in (5, 13, 9)]
    steps = 24
    free = [solo(glimmer, prompt, steps).ids[0] for prompt in prompts]
    stop = next(t for t in free[0][2:] if t not in free[1] and t not in free[2])
    expected = [solo(glimmer, prompt, steps, stop) for prompt in prompts]

    width = max(len(prompt) for prompt in prompts)
    embeds = torch.stack([F.pad(p, (0, 0, width - len(p), 0)) for p in prompts])
    mask = torch.tensor([[0] * (width - len(p)) + [1] * len(p) for p in prompts])
    batched = generate(glimmer, embeds, mask, steps, stop)

    assert batched.ids == [one.ids[0] for one in expected]
    assert batched.finished == [True, False, False]
    assert len(batched.ids[0]) < len(batched.ids[1]) == steps
    assert all(len(set(ids)) > 3 for ids in free)


def test_a_batch_scores_each_sample_as_if_it_ran_alone(
    glimmer: FrozenGlimmer, projector: AudioProjector, tiny_encoder
) -> None:
    records = [RECORDS[0], RECORDS[1], Record("d", "a third sample", "answer", "eval", RESPONSE)]
    clips = [tone(WINDOW, 0), tone(1600, 1), tone(9600, 2)]
    histories = [(("hi", "hello"),), (), (("one", "two"), ("three", "four"))]
    kwargs = {"max_new_tokens": 24, "with_text_baseline": True}

    def run(rows, audio, earlier, index=0):
        return evaluate_batch(
            glimmer,
            FakeTokenizer(),
            projector,
            *tiny_encoder,
            rows,
            audio,
            histories=earlier,
            index=index,
            **kwargs,
        )

    batched = run(records, clips, histories)
    alone = [
        run([record], [clip], [history])[0]
        for record, clip, history in zip(records, clips, histories, strict=True)
    ]

    for together, single in zip(batched, alone, strict=True):
        assert together.response == single.response
        assert together.baseline == single.baseline
        assert together.new_tokens == single.new_tokens == 24
        assert together.kl == pytest.approx(single.kl, rel=1e-4)
        assert together.audio_seconds == single.audio_seconds
    assert len({row.response for row in batched}) == 3

    batch = make_batch(
        records[:1],
        [encode(*tiny_encoder, clips[:1])[0].float()],
        FakeTokenizer(),
        projector.audio_token_count,
        histories=histories[:1],
    )
    with torch.no_grad():
        assert batched[0].kl == pytest.approx(
            distill_loss(glimmer, projector, batch).kl.item(), rel=1e-4
        )


def test_a_batch_reports_audio_length_baseline_and_its_own_timings(
    glimmer: FrozenGlimmer, projector: AudioProjector, tiny_encoder
) -> None:
    rows = RECORDS[:2]
    clips = [tone(WINDOW), tone(1600)]

    def run(with_text_baseline):
        return evaluate_batch(
            glimmer,
            FakeTokenizer(),
            projector,
            *tiny_encoder,
            rows,
            clips,
            histories=[(), ()],
            index=7,
            max_new_tokens=6,
            with_text_baseline=with_text_baseline,
        )

    plain, compared = run(False), run(True)
    for row, other in zip(plain, compared, strict=True):
        assert row.baseline is None
        assert row.response == other.response
        assert isinstance(other.baseline, Baseline)
        assert other.baseline.response.startswith("<|start|>assistant")
        assert (row.batch_index, row.batch_size) == (7, 2)
        assert row.batch_encode_s > 0
        assert 0 < row.batch_prefill_s <= row.batch_generate_s
        assert row.new_tokens == 6
    assert [row.audio_seconds for row in plain] == [2.0, 0.1]
    assert plain[0].reference_words == 5
    assert plain[0].wer == plain[0].edits / 5
    assert plain[0].batch_prefill_s == plain[1].batch_prefill_s


def test_results_come_back_in_record_order_though_batches_group_by_prompt_length(
    glimmer: FrozenGlimmer, projector: AudioProjector, tiny_encoder, audio_manifest: Path
) -> None:
    records = RECORDS
    emitted: list[list[str]] = []

    results = evaluate.evaluate(
        glimmer,
        FakeTokenizer(),
        projector,
        *tiny_encoder,
        records,
        resolve_audio(records, [audio_manifest]),
        histories=[()] * 3,
        batch_size=2,
        max_new_tokens=3,
        with_text_baseline=False,
        emit=lambda batch: emitted.append([row.id for row in batch]),
    )

    assert [row.id for row in results] == ["a", "b", "c"]
    assert emitted == [["c", "a"], ["b"]]
    assert [(row.batch_index, row.batch_size) for row in results] == [(0, 2), (1, 1), (0, 2)]


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
        "batch_index": 0,
        "batch_size": 1,
        "batch_encode_s": 0.05,
        "batch_prefill_s": 0.1,
        "batch_generate_s": 1.0,
        "response": "",
        "baseline": None,
    }
    return SampleResult(**(fields | overrides))


def test_summary_pools_edits_over_words_and_counts_each_batch_once() -> None:
    first = {"batch_index": 0, "batch_size": 2, "batch_prefill_s": 0.3, "batch_generate_s": 2.0}
    second = {"batch_index": 1, "batch_size": 1, "batch_prefill_s": 0.1, "batch_generate_s": 1.0}
    rows = [
        result("answer", 1, 2, kl=1.0, new_tokens=10, **first),
        result("answer", 0, 8, kl=3.0, new_tokens=30, finished=False, **first),
        result("continue", 3, 10, kl=5.0, new_tokens=20, **second),
    ]
    summary = summarize(rows)
    assert summary["samples"] == 3
    assert summary["wer"] == pytest.approx(4 / 20)
    assert summary["mean_kl"] == pytest.approx(3.0)
    assert summary["finished_rate"] == pytest.approx(2 / 3)
    assert summary["median_batch_prefill_s"] == pytest.approx(0.2)
    assert summary["generated_tokens_per_s"] == pytest.approx(60 / 3.0)
    assert summary["by_task"]["answer"] == {
        "samples": 2,
        "mean_kl": 2.0,
        "wer": pytest.approx(1 / 10),
        "finished_rate": 0.5,
    }
    assert summary["by_task"]["continue"]["wer"] == pytest.approx(0.3)
    assert "baseline_finished_rate" not in summary


def test_summary_adds_the_baseline_finish_rate_only_when_every_sample_has_one() -> None:
    done = Baseline("", "", True)
    cut = Baseline("", "", False)
    both = summarize([result("answer", 0, 1, baseline=done), result("answer", 0, 1, baseline=cut)])
    assert both["baseline_finished_rate"] == 0.5
    mixed = summarize([result("answer", 0, 1, baseline=done), result("answer", 0, 1)])
    assert "baseline_finished_rate" not in mixed


@pytest.fixture
def cli_arguments(
    tiny_glimmer_dir: Path,
    projector: AudioProjector,
    tiny_whisper_dir: Path,
    audio_manifest: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> list[str]:
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
    return [
        *("--manifest", str(manifest)),
        *("--audio-manifest", str(audio_manifest)),
        *("--whisper-model", str(tiny_whisper_dir)),
        *("--glimmer", str(tiny_glimmer_dir)),
        *("--projector", str(run)),
        *("--out", str(run / "eval.jsonl")),
        *("--max-new-tokens", "5"),
        *("--batch-size", "2"),
    ]


def test_cli_writes_a_row_per_eval_sample_and_a_summary(
    cli_arguments: list[str], tmp_path: Path
) -> None:
    run = tmp_path / "run"
    out = run / "eval.jsonl"

    evaluate.main([*cli_arguments, *("--latency-samples", "1"), "--with-text-baseline"])

    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert [(row["id"], row["task"]) for row in rows] == [("a", "answer"), ("b", "continue")]
    assert all(row["new_tokens"] <= 5 and row["kl"] > 0 for row in rows)
    assert all(row["baseline"]["response"] for row in rows)
    assert [row["audio_seconds"] for row in rows] == [2.0, 0.1]
    assert [row["batch_size"] for row in rows] == [2, 2]
    assert all(row["batch_encode_s"] > 0 for row in rows)
    summary = json.loads((run / "eval.summary.json").read_text(encoding="utf-8"))
    assert summary["samples"] == 2
    assert summary["history"] == {"prob": 0.0, "turns": 3, "same_task": 0.5, "seed": 0}
    assert set(summary["by_task"]) == {"answer", "continue"}
    assert summary["median_batch_prefill_s"] == rows[0]["batch_prefill_s"] > 0
    assert summary["generated_tokens_per_s"] > 0
    assert summary["mean_kl"] == pytest.approx(sum(row["kl"] for row in rows) / 2)
    single = summary["single_request"]
    assert single["samples"] == 1
    assert (
        single["median_time_to_first_token_with_encoder_s"]
        > single["median_time_to_first_token_s"]
        > 0
    )


def test_cli_in_echo_mode_starts_each_reasoning_like_serving_and_says_so(
    cli_arguments: list[str], tmp_path: Path
) -> None:
    evaluate.main([*cli_arguments, *("--latency-samples", "0"), "--echo"])

    rows = [json.loads(line) for line in (tmp_path / "run" / "eval.jsonl").read_text().splitlines()]
    assert all(row["response"].startswith(GENERATION_PROMPT + PREFILL) for row in rows)
    summary = json.loads((tmp_path / "run" / "eval.summary.json").read_text(encoding="utf-8"))
    assert summary["echo"] is True


def test_cli_runs_samples_after_history_and_records_the_setting(
    cli_arguments: list[str], tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    seen: list[tuple] = []
    real_evaluate_batch = evaluate.evaluate_batch

    def spy(*args, histories, **kwargs):
        seen.extend(histories)
        return real_evaluate_batch(*args, histories=histories, **kwargs)

    monkeypatch.setattr(evaluate, "evaluate_batch", spy)
    arguments = [
        *cli_arguments,
        *("--history-prob", "1"),
        *("--history-turns", "1"),
        *("--history-same-task", "0"),
        *("--latency-samples", "1"),
    ]
    evaluate.main(arguments)

    assert seen[:2] == [(("once upon a time", "yes"),), (("what is two plus two", "yes"),)]
    assert seen[2:] == seen[:1]
    summary = json.loads((tmp_path / "run" / "eval.summary.json").read_text(encoding="utf-8"))
    assert summary["history"] == {"prob": 1.0, "turns": 1, "same_task": 0.0, "seed": 0}
    rows = [json.loads(line) for line in (tmp_path / "run" / "eval.jsonl").read_text().splitlines()]
    evaluate.main(cli_arguments)
    plain = [
        json.loads(line) for line in (tmp_path / "run" / "eval.jsonl").read_text().splitlines()
    ]
    assert [r["kl"] for r in rows] != [r["kl"] for r in plain]


def test_batches_group_by_prompt_length_including_history(
    projector: AudioProjector,
) -> None:
    records = RECORDS[:2]
    clips = [tone(1600), tone(1600)]
    long = (("a long earlier question", "a long earlier answer"),)
    kwargs = {"projector": projector, "tokenizer": FakeTokenizer(), "batch_size": 1}
    assert evaluate.plan_batches(records, clips, [long, ()], **kwargs) == [[1], [0]]
    assert evaluate.plan_batches(records, clips, [(), long], **kwargs) == [[0], [1]]


def test_cli_refuses_a_projector_trained_for_another_hidden_size(
    tiny_glimmer_dir: Path, tmp_path: Path
) -> None:
    run = tmp_path / "run"
    run.mkdir()
    (run / PROJECTOR_CONFIG).write_text(json.dumps(PROJECTOR.__dict__ | {"text_dim": HIDDEN + 1}))
    with pytest.raises(ValueError, match="text_dim"):
        evaluate.load_projector(run, HIDDEN, torch.device("cpu"))


def test_cli_evaluates_noisy_audio_reproducibly_and_records_the_settings(
    cli_arguments: list[str], tmp_path: Path
) -> None:
    run = tmp_path / "run"
    noise = write_noise_manifest(tmp_path / "noise", {"hiss": ("noise", 4000)})

    def kls_of(extra: list[str]) -> list[float]:
        evaluate.main([*cli_arguments, *extra, *("--latency-samples", "0")])
        rows = [json.loads(line) for line in (run / "eval.jsonl").read_text().splitlines()]
        return [row["kl"] for row in rows]

    def summary() -> dict:
        return json.loads((run / "eval.summary.json").read_text(encoding="utf-8"))

    clean = kls_of([])
    assert summary()["augment"] == {"prob": 0.0, "seed": 0, "noise_manifest": None}
    noisy = ["--noise-manifest", str(noise), "--augment-prob", "1"]
    first, second = kls_of(noisy), kls_of(noisy)

    assert summary()["augment"] == {"prob": 1.0, "seed": 0, "noise_manifest": str(noise)}
    assert first == second != clean
