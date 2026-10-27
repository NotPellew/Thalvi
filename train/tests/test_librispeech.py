import json
from pathlib import Path

import numpy as np
import pytest
import soundfile

from thalvi_train.librispeech import main, sentence_case
from thalvi_train.manifest import audio_path, read_manifest

RATE = 8000


@pytest.mark.parametrize(
    ("transcript", "expected"),
    [
        ("HELLO WORLD", "Hello world"),
        ("WHEN I SAW HIM I SAID I'M SURE I'LL GO", "When I saw him I said I'm sure I'll go"),
        ("I'VE SAID IT AND I'D SAY IT AGAIN", "I've said it and I'd say it again"),
        ("THIS IS IT", "This is it"),
        ("HIS LIFE", "His life"),
        ("DON'T TELL ME", "Don't tell me"),
        ("I", "I"),
    ],
)
def test_sentence_case(transcript: str, expected: str) -> None:
    assert sentence_case(transcript) == expected


def write_chapter(subset: Path, speaker: str, chapter: str, seconds: dict[str, float]) -> None:
    folder = subset / speaker / chapter
    folder.mkdir(parents=True)
    lines = []
    for number, duration in seconds.items():
        utterance = f"{speaker}-{chapter}-{number}"
        soundfile.write(folder / f"{utterance}.flac", np.zeros(int(duration * RATE)), RATE)
        lines.append(f"{utterance} SAMPLE {number} I'M HERE")
    (folder / f"{speaker}-{chapter}.trans.txt").write_text("\n".join(lines) + "\n")


@pytest.fixture
def corpus(tmp_path: Path) -> Path:
    root = tmp_path / "data" / "LibriSpeech"
    write_chapter(root / "train-clean-100", "11", "22", {"0000": 1.0, "0001": 21.0})
    write_chapter(root / "dev-clean", "33", "44", {"0000": 2.0})
    write_chapter(root / "test-other", "55", "66", {"0000": 2.0})
    write_chapter(root / "unknown-subset", "77", "88", {"0000": 2.0})
    return tmp_path / "data"


def test_writes_continue_samples_with_splits_and_skips_long_clips(
    corpus: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    out = tmp_path / "out" / "manifest.jsonl"
    assert main([str(corpus), str(out)]) == 0
    samples = read_manifest(out)
    assert [(s.id, s.split, s.voice) for s in samples] == [
        ("ls-33-44-0000", "eval", "33"),
        ("ls-55-66-0000", "eval", "55"),
        ("ls-11-22-0000", "train", "11"),
    ]
    assert all(s.task == "continue" and s.source == "librispeech" for s in samples)
    assert samples[0].text == "Sample 0000 I'm here"
    assert all(audio_path(out, s).is_file() for s in samples)
    assert "1 clips over 20 s" in capsys.readouterr().out


def test_accepts_the_librispeech_directory_itself(corpus: Path, tmp_path: Path) -> None:
    out = tmp_path / "manifest.jsonl"
    assert main([str(corpus / "LibriSpeech"), str(out)]) == 0
    assert len(read_manifest(out)) == 3
    assert json.loads(out.read_text().splitlines()[0])["audio"].startswith("data/LibriSpeech/")
