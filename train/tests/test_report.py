import json
from pathlib import Path

import pytest

from thalvi_train import report
from thalvi_train.report import Row, pooled_wer, score_file, table


def write_rows(path: Path, pairs: list[tuple[str, str]]) -> Path:
    path.write_text(
        "".join(json.dumps({"text": t, "transcript": h}) + "\n" for t, h in pairs),
        encoding="utf-8",
    )
    return path


def digits(text: str) -> str:
    return text.lower().replace("three hundred dollars", "$300").replace("uh ", "")


def test_rates_pool_edits_over_all_reference_words() -> None:
    pairs = [("a b c", "a b c"), ("d", "e")]
    assert pooled_wer(pairs, str.split) == pytest.approx(1 / 4)


def test_the_normalized_rate_forgives_number_formatting_and_fillers(tmp_path: Path) -> None:
    rows = write_rows(tmp_path / "r.jsonl", [("Pay uh three hundred dollars", "Pay $300")])

    row = score_file("asr", rows, digits)

    assert row.samples == 1
    assert row.wer == pytest.approx(4 / 5)
    assert row.normalized_wer == 0.0


def test_the_table_prints_percentages() -> None:
    assert table([Row("v3", 300, 0.0216, 0.0151)]).splitlines()[-1] == (
        "| v3 | 300 | 2.2% | 1.5% |"
    )


def test_cli_scores_basic_labels_with_the_basic_normalizer(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    en = write_rows(tmp_path / "en.jsonl", [("uh three hundred dollars", "$300")])
    de = write_rows(tmp_path / "de.jsonl", [("uh three hundred dollars", "$300")])

    report.main([f"en={en}", f"de={de}", "--basic", "de"], normalizers=(digits, str.lower))

    lines = capsys.readouterr().out.splitlines()
    assert lines[2].endswith("| 0.0% |")
    assert lines[3].endswith("| 100.0% |")


def test_cli_rejects_an_entry_without_a_label(tmp_path: Path) -> None:
    with pytest.raises(SystemExit):
        report.main([str(tmp_path / "rows.jsonl")], normalizers=(str.lower, str.lower))
