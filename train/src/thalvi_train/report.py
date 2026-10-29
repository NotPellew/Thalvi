"""Report CLI: word error rates of several result files, raw and normalized, as a Markdown table.

Run as `python -m thalvi_train.report label=rows.jsonl ...`, with the per-sample rows
of `evaluate` or `cascade`; both carry each sample's `text` and `transcript`.

The raw rate compares lowercased words without punctuation, as `evaluate` does. It
counts "$300" against "three hundred dollars" and a dropped "uh" as errors, which
penalises a recognizer that formats numbers and drops fillers, as Muse Voice
Transcribe does, against references that spell them out. The normalized rate first
applies Whisper's English text normalizer to both sides, the usual fix for this:
numbers become digits, fillers and spelling variants go. Labels named with `--basic`
(German, say) get Whisper's language-neutral normalizer instead.
"""

import argparse
import json
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path

from thalvi_train.evaluate import edit_distance, normalize

Normalizer = Callable[[str], str]

WHISPER_MODEL = "openai/whisper-large-v3-turbo"
SPELLING_FILE = "normalizer.json"


@dataclass(frozen=True)
class Row:
    """One result file's line of the table."""

    label: str
    samples: int
    wer: float
    normalized_wer: float


def pooled_wer(pairs: Sequence[tuple[str, str]], words: Callable[[str], list[str]]) -> float:
    """Edits over reference words, pooled over `(reference, hypothesis)` pairs, split by `words`."""
    edits = total = 0
    for reference, hypothesis in pairs:
        expected = words(reference)
        edits += edit_distance(expected, words(hypothesis))
        total += len(expected)
    return edits / total if total else 0.0


def score_file(label: str, path: Path, normalizer: Normalizer) -> Row:
    """The `Row` of the per-sample rows at `path`."""
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]
    pairs = [(r["text"], r["transcript"]) for r in rows]
    return Row(
        label=label,
        samples=len(rows),
        wer=pooled_wer(pairs, normalize),
        normalized_wer=pooled_wer(pairs, lambda text: normalizer(text).split()),
    )


def table(rows: Sequence[Row]) -> str:
    """`rows` as a Markdown table, rates in percent to one decimal."""
    lines = ["| Result | Samples | WER | Normalized WER |", "|---|---|---|---|"]
    lines += [
        f"| {r.label} | {r.samples} | {r.wer * 100:.1f}% | {r.normalized_wer * 100:.1f}% |"
        for r in rows
    ]
    return "\n".join(lines)


def whisper_normalizers() -> tuple[Normalizer, Normalizer]:
    """Whisper's English normalizer, with its spelling map from the hub, and its basic one."""
    from huggingface_hub import hf_hub_download
    from transformers.models.whisper.english_normalizer import (
        BasicTextNormalizer,
        EnglishTextNormalizer,
    )

    spelling = json.loads(Path(hf_hub_download(WHISPER_MODEL, SPELLING_FILE)).read_text())
    return EnglishTextNormalizer(spelling), BasicTextNormalizer()


def _parse_entry(entry: str) -> tuple[str, Path]:
    label, sep, path = entry.partition("=")
    if not sep or not label or not path:
        raise argparse.ArgumentTypeError(f"expected label=path, got {entry!r}")
    return label, Path(path)


def main(argv: list[str] | None = None, normalizers: tuple[Normalizer, Normalizer] | None = None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("entries", nargs="+", type=_parse_entry, metavar="label=rows.jsonl")
    parser.add_argument(
        "--basic", action="append", default=[], help="label scored with the basic normalizer"
    )
    args = parser.parse_args(argv)
    english, basic = normalizers or whisper_normalizers()
    rows = [
        score_file(label, path, basic if label in args.basic else english)
        for label, path in args.entries
    ]
    print(table(rows))


if __name__ == "__main__":
    main()
