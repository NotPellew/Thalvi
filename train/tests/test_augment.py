import json
from pathlib import Path

import numpy as np
import pytest
import soundfile
from tiny_noise import write_noise_manifest
from tiny_whisper import tone

from thalvi_train import augment
from thalvi_train.augment import (
    BABBLE_SNR_DB,
    NOISE_SNR_DB,
    Augmenter,
    ClipCache,
    NoiseManifestError,
    add_reverb,
    build_augmenter,
    fit_length,
    read_noise_manifest,
    rms,
    room_impulse_response,
    scaled_to_snr,
)

SECOND = 16_000


def speech(samples: int = 3 * SECOND, seed: int = 0) -> np.ndarray:
    return tone(samples, seed) * 3


def snr_db(clean: np.ndarray, mixed: np.ndarray) -> float:
    return float(20 * np.log10(rms(clean) / rms(mixed - clean)))


@pytest.fixture
def training_audio(tmp_path: Path) -> list[Path]:
    paths = []
    for index in range(4):
        path = tmp_path / "train" / f"{index}.wav"
        path.parent.mkdir(exist_ok=True)
        soundfile.write(path, speech(SECOND, index + 10), SECOND)
        paths.append(path)
    return paths


@pytest.fixture
def noise_manifest(tmp_path: Path) -> Path:
    return write_noise_manifest(
        tmp_path / "noise",
        {
            "hiss": ("noise", SECOND),
            "tune": ("music", 5 * SECOND),
            "talk": ("speech", SECOND // 2),
            "chat": ("speech", 2 * SECOND),
        },
    )


@pytest.fixture
def augmenter(noise_manifest: Path, training_audio: list[Path]) -> Augmenter:
    return Augmenter(read_noise_manifest(noise_manifest), training_audio, prob=1.0, seed=7)


@pytest.mark.parametrize("length", [SECOND // 4, 3 * SECOND, 10 * SECOND])
@pytest.mark.parametrize("target", [0.0, 5.0, 12.5, 20.0])
def test_background_lands_at_the_target_snr_whether_it_loops_or_crops(
    target: float, length: int
) -> None:
    clip = speech()
    background = fit_length(tone(length, 5) * 0.3, len(clip), start=length // 3)
    mixed = clip + scaled_to_snr(clip, background, target)
    assert snr_db(clip, mixed) == pytest.approx(target, abs=0.5)


def test_a_silent_clip_or_background_gets_nothing_mixed_in() -> None:
    clip = speech()
    silence = np.zeros_like(clip)
    assert not scaled_to_snr(clip, silence, 10).any()
    assert not scaled_to_snr(silence, clip, 10).any()


def test_fit_length_loops_a_short_clip_and_crops_a_long_one_circularly() -> None:
    ramp = np.arange(5, dtype=np.float32)
    assert fit_length(ramp, 7, start=3).tolist() == [3, 4, 0, 1, 2, 3, 4]
    assert fit_length(ramp, 2, start=1).tolist() == [1, 2]


def test_reverb_keeps_length_and_loudness_but_changes_the_signal() -> None:
    clip = speech()
    for seed, rt60 in enumerate([0.15, 0.4, 0.8]):
        room = room_impulse_response(rt60, np.random.default_rng(seed))
        wet = add_reverb(clip, room)
        assert wet.shape == clip.shape and wet.dtype == np.float32
        assert rms(wet) == pytest.approx(rms(clip), rel=1e-3)
        assert not np.allclose(wet, clip)


def test_room_impulse_response_is_a_direct_impulse_then_a_tail_decaying_over_the_rt60() -> None:
    room = room_impulse_response(0.5, np.random.default_rng(1))
    assert len(room) == 8000
    assert room[0] == 1.0
    assert np.sum(room[1:] ** 2) == pytest.approx(augment.TAIL_TO_DIRECT_ENERGY)
    early, late = np.sum(room[1:2000] ** 2), np.sum(room[-2000:] ** 2)
    assert late < early * 1e-2


def test_reverb_of_silence_is_silence() -> None:
    silence = np.zeros(SECOND, dtype=np.float32)
    assert not add_reverb(silence, room_impulse_response(0.3, np.random.default_rng(0))).any()


def test_probability_zero_returns_every_clip_untouched(
    noise_manifest: Path, training_audio: list[Path]
) -> None:
    off = Augmenter(read_noise_manifest(noise_manifest), training_audio, prob=0.0, seed=7)
    clip = speech()
    assert all(off(clip, index, epoch) is clip for index in range(4) for epoch in range(3))


def test_a_share_of_clips_matching_the_probability_is_augmented(
    noise_manifest: Path, training_audio: list[Path]
) -> None:
    half = Augmenter(read_noise_manifest(noise_manifest), training_audio, prob=0.5, seed=7)
    clip = speech()
    changed = sum(half(clip, index % 4, index // 4) is not clip for index in range(400))
    assert 160 < changed < 240


def test_same_seed_epoch_and_index_give_the_same_clip_and_others_differ(
    augmenter: Augmenter, noise_manifest: Path, training_audio: list[Path]
) -> None:
    clip = speech()
    again = Augmenter(read_noise_manifest(noise_manifest), training_audio, prob=1.0, seed=7)
    assert np.array_equal(augmenter(clip, 2, 1), augmenter(clip, 2, 1))
    assert np.array_equal(augmenter(clip, 2, 1), again(clip, 2, 1))
    assert not np.array_equal(augmenter(clip, 2, 1), augmenter(clip, 3, 1))
    assert not np.array_equal(augmenter(clip, 2, 1), augmenter(clip, 2, 2))
    other_seed = Augmenter(read_noise_manifest(noise_manifest), training_audio, prob=1.0, seed=8)
    assert not np.array_equal(augmenter(clip, 2, 1), other_seed(clip, 2, 1))


def test_output_has_the_input_length_and_stays_within_full_scale(augmenter: Augmenter) -> None:
    loud = np.clip(speech(2 * SECOND + 123, 3) * 8, -1, 1).astype(np.float32)
    for index in range(60):
        out = augmenter(loud, index % 4, index)
        assert out.shape == loud.shape and out.dtype == np.float32
        assert np.abs(out).max() <= 1.0
        assert np.isfinite(out).all()


def test_babble_never_draws_the_samples_own_audio_but_does_draw_the_others(
    tmp_path: Path, training_audio: list[Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    only_speech = write_noise_manifest(tmp_path / "talk", {"talk": ("speech", SECOND)})
    decoded: list[Path] = []
    real_decode = augment.decode_audio

    def spy(path):
        decoded.append(path)
        return real_decode(path)

    monkeypatch.setattr(augment, "decode_audio", spy)
    babble = Augmenter(
        read_noise_manifest(only_speech), training_audio, prob=1.0, seed=1, cache_samples=0
    )
    clip = speech(SECOND)
    for epoch in range(80):
        babble(clip, 2, epoch)

    assert training_audio[2] not in decoded
    assert {training_audio[0], training_audio[1], training_audio[3]} <= set(decoded)
    assert any(path.parent.name == "talk" for path in decoded)


def test_every_background_is_mixed_in_at_an_snr_within_its_range(
    tmp_path: Path, training_audio: list[Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    seen: list[float] = []
    real = augment.scaled_to_snr

    def spy(clip, background, snr):
        seen.append(snr)
        return real(clip, background, snr)

    monkeypatch.setattr(augment, "scaled_to_snr", spy)
    clip = speech(SECOND)
    hiss = write_noise_manifest(tmp_path / "hiss", {"hiss": ("noise", SECOND)})
    noisy = Augmenter(read_noise_manifest(hiss), training_audio[:1], prob=1.0, seed=2)
    for index in range(50):
        noisy(clip, 0, index)
    assert len(seen) == 50
    assert min(seen) >= NOISE_SNR_DB[0] and max(seen) <= NOISE_SNR_DB[1]
    assert max(seen) - min(seen) > 10

    seen.clear()
    babble = Augmenter([], training_audio, prob=1.0, seed=2)
    for index in range(50):
        babble(clip, 0, index)
    assert 50 <= len(seen) <= 150
    assert min(seen) >= BABBLE_SNR_DB[0] and max(seen) <= BABBLE_SNR_DB[1]


def test_a_silent_clip_stays_silent_through_the_whole_chain(augmenter: Augmenter) -> None:
    silence = np.zeros(SECOND, dtype=np.float32)
    assert all(not augmenter(silence, 1, epoch).any() for epoch in range(20))


def test_clip_cache_evicts_the_least_recently_used_and_stays_within_its_budget(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    paths = {}
    for name in "abc":
        paths[name] = tmp_path / f"{name}.wav"
        soundfile.write(paths[name], tone(40), SECOND)
    decoded: list[str] = []
    real = augment.decode_audio
    monkeypatch.setattr(augment, "decode_audio", lambda p: decoded.append(p.stem) or real(p))
    cache = ClipCache(max_samples=100)

    for name in "abac":
        cache.get(paths[name])
    assert decoded == ["a", "b", "c"]
    cache.get(paths["a"])
    cache.get(paths["b"])
    assert decoded == ["a", "b", "c", "b"]
    assert cache._samples <= 100
    assert cache.get(paths["a"]).flags.writeable is False


def test_clip_cache_serves_a_clip_bigger_than_its_budget_without_keeping_it(tmp_path: Path) -> None:
    path = tmp_path / "big.wav"
    soundfile.write(path, tone(500), SECOND)
    cache = ClipCache(max_samples=100)
    assert len(cache.get(path)) == 500
    assert cache._samples == 0


def test_manifest_clips_resolve_against_the_manifest_directory(noise_manifest: Path) -> None:
    clips = read_noise_manifest(noise_manifest)
    assert [(c.path.name, c.category, c.seconds) for c in clips] == [
        ("hiss.wav", "noise", 1.0),
        ("tune.wav", "music", 5.0),
        ("talk.wav", "speech", 0.5),
        ("chat.wav", "speech", 2.0),
    ]
    assert all(c.path.parent == noise_manifest.parent for c in clips)


GOOD = {"path": "hiss.wav", "category": "noise", "seconds": 1.0}


@pytest.mark.parametrize(
    ("row", "message"),
    [
        ("not json", "not JSON"),
        ("[1]", "must be an object"),
        (GOOD | {"path": ""}, "path must be"),
        (GOOD | {"path": "/etc/passwd"}, "must stay inside"),
        (GOOD | {"path": "../hiss.wav"}, "must stay inside"),
        (GOOD | {"category": "silence"}, "unknown category"),
        (GOOD | {"seconds": 0}, "positive number"),
        (GOOD | {"seconds": "1"}, "positive number"),
        (GOOD | {"seconds": True}, "positive number"),
        (GOOD | {"seconds": float("inf")}, "positive number"),
        (GOOD | {"path": "gone.wav"}, "does not exist"),
    ],
)
def test_manifest_rows_are_checked_where_they_enter(
    tmp_path: Path, row: object, message: str
) -> None:
    (tmp_path / "hiss.wav").write_bytes(b"")
    manifest = tmp_path / "noise.jsonl"
    manifest.write_text(
        json.dumps(GOOD) + "\n\n" + (row if isinstance(row, str) else json.dumps(row)) + "\n",
        encoding="utf-8",
    )
    with pytest.raises(NoiseManifestError, match=rf"noise.jsonl:3: .*{message}"):
        read_noise_manifest(manifest)


def test_an_empty_manifest_is_refused(tmp_path: Path) -> None:
    manifest = tmp_path / "noise.jsonl"
    manifest.write_text("\n", encoding="utf-8")
    with pytest.raises(NoiseManifestError, match="no clips"):
        read_noise_manifest(manifest)


def test_building_an_augmenter_validates_the_manifest_even_when_off(
    noise_manifest: Path, training_audio: list[Path], tmp_path: Path
) -> None:
    assert build_augmenter(noise_manifest, 0.0, training_audio, 0) is None
    assert build_augmenter(None, 0.0, training_audio, 0) is None
    on = build_augmenter(None, 0.25, training_audio, 3)
    assert on is not None and (on.prob, on.seed) == (0.25, 3)

    bad = tmp_path / "bad.jsonl"
    bad.write_text("{}\n", encoding="utf-8")
    with pytest.raises(NoiseManifestError):
        build_augmenter(bad, 0.0, training_audio, 0)
    with pytest.raises(ValueError, match="between 0 and 1"):
        build_augmenter(None, 1.5, training_audio, 0)
    with pytest.raises(ValueError, match="nothing to mix in"):
        build_augmenter(None, 1.0, training_audio[:1], 0)
