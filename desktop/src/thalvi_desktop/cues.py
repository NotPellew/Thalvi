"""Owns the musical earcons: their names, per-cue switches, and synthesized waveforms."""

from dataclasses import dataclass
from enum import StrEnum

import numpy as np

RATE = 24_000


class Cue(StrEnum):
    WAKE = "wake"
    PROVISIONAL = "provisional"
    CANCELLED = "cancelled"
    ACCEPTED = "accepted"
    ERROR = "error"


@dataclass(frozen=True)
class Cues:
    wake: bool = True
    provisional: bool = True
    cancelled: bool = True
    accepted: bool = True
    error: bool = True

    def enabled(self, cue: Cue) -> bool:
        return getattr(self, cue.value)


SEQUENCES: dict[Cue, tuple[tuple[float, float], ...]] = {
    Cue.WAKE: ((523.25, 0.09), (783.99, 0.14)),
    Cue.PROVISIONAL: ((659.25, 0.07), (880.0, 0.07)),
    Cue.CANCELLED: ((783.99, 0.09), (523.25, 0.14)),
    Cue.ACCEPTED: ((523.25, 0.08), (659.25, 0.08), (783.99, 0.16)),
    Cue.ERROR: ((261.63, 0.14), (369.99, 0.20)),
}


def synthesize(cue: Cue) -> np.ndarray:
    notes = []
    for frequency, duration in SEQUENCES[cue]:
        t = np.arange(int(RATE * duration)) / RATE
        notes.append(0.15 * np.sin(2 * np.pi * frequency * t) * np.sin(np.pi * t / duration))
    return np.concatenate(notes).astype(np.float32)
