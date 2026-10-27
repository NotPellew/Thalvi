import tarfile
from collections import Counter
from dataclasses import replace
from pathlib import Path

import pytest
import soundfile
from corpus_files import wav_bytes, write_fleurs, write_musan, write_voxpopuli

from thalvi_train import augment, corpora
from thalvi_train.manifest import ManifestError, audio_path, read_manifest

VOXPOPULI_ROWS = [
    {
        "id": "20180418-0900-PLENARY-3-en_20180418-08:50:36_17",
        "raw": "If you do not address this, we fail.",
        "normalized": "if you do not address this we fail.",
        "speaker": "124737",
        "seconds": 1.0,
    },
    {
        "id": "b-1",
        "raw": "",
        "normalized": "i said i'm here",
        "speaker": "None",
        "seconds": 0.5,
        "rate": 8000,
    },
    {"id": "b-2", "raw": "", "normalized": "", "speaker": "124737", "seconds": 0.5},
    {"id": "b-3", "raw": "Far too long.", "normalized": "far", "speaker": "9", "seconds": 21.0},
]


def test_voxpopuli_rows_become_flac_clips_with_a_manifest(tmp_path: Path) -> None:
    parquet = write_voxpopuli(tmp_path / "vp.parquet", VOXPOPULI_ROWS)
    clips = corpora.voxpopuli_clips(parquet, lambda speaker: "eval" if speaker == "9" else "train")
    written = corpora.write_clips(clips, tmp_path / "voxpopuli", "voxpopuli")

    manifest = tmp_path / "voxpopuli" / "manifest.jsonl"
    first, second = read_manifest(manifest)
    assert (first.id, first.text, first.voice, first.split) == (
        "vp-20180418-0900-PLENARY-3-en_20180418-08-50-36_17",
        "If you do not address this, we fail.",
        "124737",
        "train",
    )
    assert (second.id, second.text, second.voice) == ("vp-b-1", "I said I'm here", "unknown")
    assert all(s.task == "continue" and s.source == "voxpopuli" for s in (first, second))
    assert (written.too_long, written.no_text) == (1, 1)
    assert written.seconds["vp-b-1"] == pytest.approx(0.5)
    for sample in (first, second):
        info = soundfile.info(audio_path(manifest, sample))
        assert (info.format, info.samplerate, info.channels) == ("FLAC", 16_000, 1)


def test_fleurs_rows_are_identified_by_audio_file_not_by_the_repeating_sentence_id(
    tmp_path: Path,
) -> None:
    rows = [
        {"stem": "111", "sentence": 7, "text": "Hallo Welt.", "gender": 1, "seconds": 1.0},
        {"stem": "222", "sentence": 7, "text": "Hallo Welt.", "gender": 0, "seconds": 1.0},
    ]
    parquet = write_fleurs(tmp_path / "fl.parquet", rows)
    clips = corpora.fleurs_clips(parquet, "de", "eval")
    corpora.write_clips(clips, tmp_path / "fleurs", "fleurs")
    samples = read_manifest(tmp_path / "fleurs" / "manifest.jsonl")
    assert [(s.id, s.voice, s.split, s.text) for s in samples] == [
        ("fl-de-111", "female", "eval", "Hallo Welt."),
        ("fl-de-222", "male", "eval", "Hallo Welt."),
    ]


def test_clips_with_empty_or_unreadable_audio_are_left_out_and_counted(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    good = corpora.Clip("vp-a", wav_bytes(0.5), "Hello.", "1", "train")
    empty = corpora.Clip("vp-b", wav_bytes(0.0), "Hello.", "1", "train")
    broken = corpora.Clip("vp-c", b"not audio", "Hello.", "1", "train")
    written = corpora.write_clips([good, empty, broken], tmp_path / "out", "voxpopuli")
    assert [s.id for s in written.samples] == ["vp-a"]
    assert written.no_audio == 2
    assert "vp-b: no usable audio" in capsys.readouterr().out


def test_a_repeated_id_stops_the_conversion(tmp_path: Path) -> None:
    rows = [
        {"id": "same", "raw": "a", "normalized": "a", "speaker": "1", "seconds": 0.5},
        {"id": "same", "raw": "b", "normalized": "b", "speaker": "2", "seconds": 0.5},
    ]
    parquet = write_voxpopuli(tmp_path / "vp.parquet", rows)
    with pytest.raises(ManifestError, match="duplicate id 'vp-same'"):
        corpora.write_clips(
            corpora.voxpopuli_clips(parquet, lambda _: "train"), tmp_path / "out", "voxpopuli"
        )


def test_eval_speakers_are_whole_speakers_holding_about_the_asked_clips() -> None:
    counts = {str(speaker): 1 + speaker % 5 for speaker in range(200)}
    total = sum(counts.values())
    chosen = corpora.choose_eval_speakers(counts, 0.15 * total)
    share = sum(counts[s] for s in chosen) / total
    assert 0.15 <= share < 0.18
    assert chosen == corpora.choose_eval_speakers(dict(reversed(counts.items())), 0.15 * total)


def test_the_plan_keeps_every_eval_speaker_out_of_training() -> None:
    train = Counter({"t1": 5, "t2": 5, "both": 2, "None": 9})
    accented = Counter({f"a{n}": 2 for n in range(20)} | {"both": 4, "t1": 1})
    test = Counter({"x1": 3, "x2": 3, "t2": 4, "None": 2, "a0": 1, "a1": 1, "a2": 1, "a3": 1})
    plan = corpora.plan_voxpopuli(train, accented, test, 0.25)

    assert plan.accented_eval.isdisjoint(train)
    assert sum(accented[s] for s in plan.accented_eval) / sum(accented.values()) >= 0.25
    accented_train = set(accented) - plan.accented_eval
    assert plan.test_speakers.isdisjoint(set(train) | accented_train)
    assert {"x1", "x2"} <= plan.test_speakers
    assert not {"t2", "None"} & plan.test_speakers
    assert plan.test_clips_left_out == sum(test.values()) - sum(test[s] for s in plan.test_speakers)


def test_only_the_asked_speakers_are_read(tmp_path: Path) -> None:
    rows = [
        {"id": f"c-{n}", "raw": "x", "normalized": "x", "speaker": speaker, "seconds": 0.5}
        for n, speaker in enumerate(["1", "2", "3"])
    ]
    parquet = write_voxpopuli(tmp_path / "vp.parquet", rows)
    clips = corpora.voxpopuli_clips(parquet, lambda _: "eval", {"1", "3"})
    assert [clip.voice for clip in clips] == ["1", "3"]


def test_accented_clips_of_one_speaker_all_land_in_the_same_split(tmp_path: Path) -> None:
    rows = [
        {"id": f"c-{n}", "raw": "x", "normalized": "x", "speaker": str(n % 4), "seconds": 0.5}
        for n in range(16)
    ]
    parquet = write_voxpopuli(tmp_path / "vp.parquet", rows)
    counts = corpora.speaker_counts(parquet)
    assert counts == {"0": 4, "1": 4, "2": 4, "3": 4}
    held_out = corpora.choose_eval_speakers(counts, 8)
    clips = corpora.voxpopuli_clips(parquet, lambda s: "eval" if s in held_out else "train")
    written = corpora.write_clips(clips, tmp_path / "out", "voxpopuli")
    splits: dict[str, set[str]] = {}
    for sample in written.samples:
        splits.setdefault(sample.voice, set()).add(sample.split)
    assert all(len(found) == 1 for found in splits.values())
    assert {s.split for s in written.samples} == {"train", "eval"}


def test_the_summary_counts_clips_hours_and_voices_per_group(tmp_path: Path) -> None:
    rows = [
        {"id": "a", "raw": "x", "normalized": "x", "speaker": "1", "seconds": 1.8},
        {"id": "b", "raw": "x", "normalized": "x", "speaker": "1", "seconds": 1.8},
        {"id": "c", "raw": "x", "normalized": "x", "speaker": "2", "seconds": 1.8},
        {"id": "d", "raw": "x", "normalized": "x", "speaker": "None", "seconds": 1.8},
        {"id": "e", "raw": "", "normalized": "", "speaker": "3", "seconds": 1.0},
    ]
    parquet = write_voxpopuli(tmp_path / "vp.parquet", rows)
    clips = corpora.voxpopuli_clips(parquet, lambda s: "eval" if s == "2" else "train")
    written = corpora.write_clips(clips, tmp_path / "out", "voxpopuli")
    assert corpora.summary_lines("vp", written) == [
        "vp eval: 1 clips, 0.00 h, 1 voices",
        "vp train: 3 clips, 0.00 h, 2 voices",
        "vp: 0 clips over 20 s, 1 without text and 0 without audio left out",
    ]
    assert corpora.shared_speakers(written) == set()


def test_speakers_in_both_splits_are_reported_except_the_unknown_one(tmp_path: Path) -> None:
    rows = [
        {"id": f"c-{n}", "raw": "x", "normalized": "x", "speaker": speaker, "seconds": 0.5}
        for n, speaker in enumerate(["1", "1", "2", "None", "None"])
    ]
    parquet = write_voxpopuli(tmp_path / "vp.parquet", rows)
    splits = iter(["train", "eval", "train", "train", "eval"])
    clips = (
        replace(clip, split=next(splits))
        for clip in corpora.voxpopuli_clips(parquet, lambda _: "train")
    )
    written = corpora.write_clips(clips, tmp_path / "out", "voxpopuli")
    assert corpora.shared_speakers(written) == {"1"}


def test_musan_is_listed_by_category_and_the_augmenter_reads_the_manifest(tmp_path: Path) -> None:
    root = write_musan(tmp_path)
    clips = corpora.list_musan(root)
    assert [(c.path, c.category, c.seconds) for c in clips] == [
        ("noise/free-sound/noise-0.wav", "noise", 1.0),
        ("noise/sound-bible/noise-1.wav", "noise", 2.5),
        ("music/fma/music-0.wav", "music", 3.0),
        ("speech/librivox/speech-0.wav", "speech", 0.5),
    ]
    manifest = root / "manifest.jsonl"
    assert corpora.write_noise_manifest(manifest, clips) == 4
    assert [
        (clip.path, clip.category, clip.seconds) for clip in augment.read_noise_manifest(manifest)
    ] == [(root / c.path, c.category, c.seconds) for c in clips]


@pytest.mark.parametrize(
    ("overrides", "message"),
    [
        ({"category": "rain"}, "category 'rain'"),
        ({"path": "/a.wav"}, "relative POSIX"),
        ({"seconds": 0.0}, "must be positive"),
    ],
)
def test_a_bad_noise_clip_is_rejected(overrides: dict, message: str) -> None:
    fields = {"path": "a.wav", "category": "noise", "seconds": 1.0}
    with pytest.raises(ManifestError, match=message):
        corpora.NoiseClip(**{**fields, **overrides})


def test_a_packed_tar_starts_with_the_manifest_and_appears_only_when_complete(
    tmp_path: Path,
) -> None:
    root = write_musan(tmp_path)
    corpora.write_noise_manifest(root / "manifest.jsonl", corpora.list_musan(root))
    archive = tmp_path / "archives" / "musan.tar"
    corpora.pack_tar(root, archive)

    assert [p.name for p in archive.parent.iterdir()] == ["musan.tar"]
    with tarfile.open(archive) as tar:
        names = tar.getnames()
    assert names[0] == "musan/manifest.jsonl"
    assert "musan/speech/librivox/speech-0.wav" in names
    assert len(names) == len(set(names)) == 7
