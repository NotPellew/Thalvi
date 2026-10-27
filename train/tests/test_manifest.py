import json
from pathlib import Path

import pytest

from thalvi_train.manifest import ManifestError, Sample, audio_path, read_manifest, write_manifest


def sample(**overrides: str) -> Sample:
    fields = {
        "id": "q-1",
        "audio": "audio/q-1.wav",
        "text": "Set a timer for ten minutes",
        "task": "answer",
        "split": "train",
        "voice": "af_heart",
        "source": "kokoro",
    }
    return Sample(**{**fields, **overrides})


def test_round_trips_samples_with_unicode_text(tmp_path: Path) -> None:
    path = tmp_path / "sub" / "manifest.jsonl"
    samples = [sample(), sample(id="q-2", text="Café au lait, s'il vous plaît")]
    assert write_manifest(path, samples) == 2
    assert read_manifest(path) == samples
    assert audio_path(path, samples[0]) == tmp_path / "sub" / "audio" / "q-1.wav"


@pytest.mark.parametrize(
    "overrides",
    [
        {"id": "has/slash"},
        {"id": ".hidden"},
        {"id": ""},
        {"audio": "/abs/q.wav"},
        {"audio": ""},
        {"text": "   "},
        {"voice": ""},
        {"task": "chat"},
        {"split": "dev"},
        {"source": "youtube"},
    ],
)
def test_sample_rejects_invalid_fields(overrides: dict[str, str]) -> None:
    with pytest.raises(ManifestError):
        sample(**overrides)


def test_write_rejects_duplicate_ids(tmp_path: Path) -> None:
    with pytest.raises(ManifestError, match="duplicate id"):
        write_manifest(tmp_path / "m.jsonl", [sample(), sample()])


def good_row() -> dict[str, str]:
    return {
        "id": "q-1",
        "audio": "a.wav",
        "text": "hi",
        "task": "answer",
        "split": "train",
        "voice": "v",
        "source": "kokoro",
    }


@pytest.mark.parametrize(
    ("lines", "message"),
    [
        (["{not json"], r"m\.jsonl:1: invalid JSON"),
        (["[1]"], r"m\.jsonl:1: row is not a JSON object"),
        (
            [json.dumps({k: v for k, v in good_row().items() if k != "voice"})],
            r"missing \['voice'\]",
        ),
        ([json.dumps({**good_row(), "extra": "x"})], r"unexpected \['extra'\]"),
        (
            [json.dumps(good_row()), json.dumps({**good_row(), "split": "dev"})],
            r"m\.jsonl:2: split",
        ),
        ([json.dumps(good_row()), json.dumps(good_row())], r"m\.jsonl:2: duplicate id"),
        ([json.dumps({**good_row(), "text": 5})], r"text must be a string"),
    ],
)
def test_read_names_the_line_of_a_bad_row(tmp_path: Path, lines: list[str], message: str) -> None:
    path = tmp_path / "m.jsonl"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    with pytest.raises(ManifestError, match=message):
        read_manifest(path)
