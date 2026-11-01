"""Exercises the earcon waveforms and the per-cue desktop configuration."""

import json
from dataclasses import fields
from pathlib import Path

import numpy as np
import pytest

from thalvi_desktop.config import Config
from thalvi_desktop.cues import RATE, SEQUENCES, Cue, Cues, synthesize

EXAMPLE = Path(__file__).parents[1] / "config.example.json"


def dominant(segment: np.ndarray) -> float:
    spectrum = np.abs(np.fft.rfft(segment * np.hanning(len(segment)), 1 << 16))
    return float(np.argmax(spectrum) * RATE / (1 << 16))


def segments(cue: Cue) -> list[tuple[float, np.ndarray]]:
    wave, start, result = synthesize(cue), 0, []
    for frequency, duration in SEQUENCES[cue]:
        size = int(RATE * duration)
        result.append((frequency, wave[start : start + size]))
        start += size
    return result


@pytest.mark.parametrize("cue", list(Cue))
def test_each_cue_is_a_quiet_float32_waveform_of_its_notes(cue):
    wave = synthesize(cue)
    assert wave.dtype == np.float32 and np.isfinite(wave).all()
    assert len(wave) == sum(int(RATE * duration) for _, duration in SEQUENCES[cue])
    assert 0 < np.max(np.abs(wave)) <= 0.15 and wave[0] == 0
    for frequency, segment in segments(cue):
        assert dominant(segment) == pytest.approx(frequency, abs=8)


def test_cue_contours_are_distinct_and_carry_their_meaning():
    pitches = {cue: [dominant(segment) for _, segment in segments(cue)] for cue in Cue}
    assert pitches[Cue.WAKE][0] < pitches[Cue.WAKE][1]
    assert pitches[Cue.PROVISIONAL][0] < pitches[Cue.PROVISIONAL][1]
    assert pitches[Cue.CANCELLED][0] > pitches[Cue.CANCELLED][1]
    accepted = pitches[Cue.ACCEPTED]
    assert len(accepted) == 3 and accepted == sorted(accepted)
    low, high = pitches[Cue.ERROR]
    assert high / low == pytest.approx(2 ** (6 / 12), rel=0.02)
    waves = [synthesize(cue) for cue in Cue]
    assert not any(
        len(a) == len(b) and np.allclose(a, b) for i, a in enumerate(waves) for b in waves[i + 1 :]
    )
    assert all(np.array_equal(synthesize(cue), synthesize(cue)) for cue in Cue)


def test_cues_switch_by_name():
    assert all(Cues().enabled(cue) for cue in Cue)
    assert {f.name for f in fields(Cues)} == {cue.value for cue in Cue}
    off = Cues(accepted=False)
    assert [cue for cue in Cue if not off.enabled(cue)] == [Cue.ACCEPTED]


def load(tmp_path, cues: object = None) -> Config:
    raw = json.loads(EXAMPLE.read_text())
    raw.pop("cues")
    if cues is not None:
        raw["cues"] = cues
    path = tmp_path / "config.json"
    path.write_text(json.dumps(raw))
    return Config.load(path)


def test_config_cues_default_to_enabled_and_accept_partial_blocks(tmp_path):
    assert Config.load(EXAMPLE).cues == Cues()
    assert load(tmp_path).cues == Cues()
    assert load(tmp_path, {}).cues == Cues()
    assert load(tmp_path, {"error": False, "wake": True}).cues == Cues(error=False)


@pytest.mark.parametrize(
    "cues",
    [
        {"chime": True},
        {"wake": 1},
        {"wake": "true"},
        {"wake": None},
        {"wake": 0.0},
        [],
        "all",
    ],
)
def test_config_rejects_unknown_cue_names_and_non_boolean_values(tmp_path, cues):
    with pytest.raises(ValueError, match="cues"):
        load(tmp_path, cues)
