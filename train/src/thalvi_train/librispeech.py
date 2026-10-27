"""Turns extracted LibriSpeech subsets into a manifest of read speech.

Run as `python -m thalvi_train.librispeech <root> <out.jsonl>`.
"""

import argparse
import os
import re
from collections.abc import Iterator
from pathlib import Path

import soundfile

from thalvi_train.manifest import Sample, Split, write_manifest

MAX_SECONDS = 20.0

_I_WORD = re.compile(r"(?<![\w'])i(?:'(?:m|ve|ll|d))?(?![\w'])")


def sentence_case(transcript: str) -> str:
    """Lower-cases an upper-case transcript, keeping "I" and its contractions capital."""
    text = _I_WORD.sub(lambda match: match.group().capitalize(), transcript.lower())
    return text[:1].upper() + text[1:]


def split_for(subset: str) -> Split | None:
    """`train` for `train-*` subsets, `eval` for `dev-*` and `test-*`, else `None`."""
    if subset.startswith("train-"):
        return "train"
    if subset.startswith(("dev-", "test-")):
        return "eval"
    return None


def librispeech_dir(root: Path) -> Path:
    """`root/LibriSpeech` if it exists, otherwise `root` itself."""
    nested = root / "LibriSpeech"
    return nested if nested.is_dir() else root


def collect(root: Path, manifest: Path) -> tuple[list[Sample], int]:
    """Samples for every known subset under `root`, and the number of clips skipped as too long."""
    base = librispeech_dir(root)
    samples: list[Sample] = []
    skipped = 0
    for subset_dir in sorted(p for p in base.iterdir() if p.is_dir()):
        split = split_for(subset_dir.name)
        if split is None:
            continue
        for transcripts in sorted(subset_dir.glob("*/*/*.trans.txt")):
            for utterance, text in _read_transcripts(transcripts):
                flac = transcripts.parent / f"{utterance}.flac"
                if soundfile.info(flac).duration > MAX_SECONDS:
                    skipped += 1
                    continue
                samples.append(
                    Sample(
                        id=f"ls-{utterance}",
                        audio=Path(os.path.relpath(flac, manifest.parent)).as_posix(),
                        text=sentence_case(text),
                        task="continue",
                        split=split,
                        voice=utterance.split("-")[0],
                        source="librispeech",
                    )
                )
    return samples, skipped


def _read_transcripts(path: Path) -> Iterator[tuple[str, str]]:
    for line in path.read_text(encoding="utf-8").splitlines():
        utterance, _, text = line.strip().partition(" ")
        if text:
            yield utterance, text


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="directory holding (or being) LibriSpeech/")
    parser.add_argument("out", type=Path, help="manifest JSONL to write")
    args = parser.parse_args(argv)
    samples, skipped = collect(args.root, args.out)
    write_manifest(args.out, samples)
    print(f"wrote {len(samples)} samples to {args.out}; {skipped} clips over {MAX_SECONDS:g} s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
