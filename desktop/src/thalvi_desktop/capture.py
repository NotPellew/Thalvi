"""Owns captured text and the configurable ending commands that dispatch it."""

import re
from dataclasses import dataclass, field
from enum import StrEnum


class Ending(StrEnum):
    PASTE = "paste"
    SUBMIT = "submit"
    TASK = "task"
    SCRATCH = "scratch"
    CANCEL = "cancel"


DEFAULT_ENDINGS = {
    "paste that": Ending.PASTE,
    "finish dictation": Ending.PASTE,
    "submit": Ending.SUBMIT,
    "do that": Ending.TASK,
    "scratch that": Ending.SCRATCH,
    "never mind": Ending.CANCEL,
}

ARTIFACT = (
    r"(?:\[[^\[\]\r\n]{1,32}\]|\([^()\r\n]{1,32}\)|"
    r"<\|[^<>\r\n]{1,32}\|>|u+h+|u+m+|h+m+|m+h+|a+h+|e+r+m+)"
)
TRAILING_ARTIFACTS = re.compile(rf"[\s.!?,;:]*(?:{ARTIFACT}[\s.!?,;:]*){{1,2}}$", re.I)


def trailing_artifacts(text: str) -> bool:
    return TRAILING_ARTIFACTS.fullmatch(text) is not None


@dataclass(frozen=True)
class Request:
    ending: Ending
    text: str
    pause_confirmed: bool = False


@dataclass
class Capture:
    endings: dict[str, Ending] = field(default_factory=lambda: dict(DEFAULT_ENDINGS))
    pieces: list[str] = field(default_factory=list)
    finished: bool = False
    scratched: bool = False

    @property
    def text(self) -> str:
        return " ".join(self.pieces)

    def ending(self, combined: str):
        for phrase in sorted(self.endings, key=len, reverse=True):
            words = phrase.split()
            pattern = r"[\s.!?,;:]+".join(map(re.escape, words))
            match = re.search(
                r"(?:^|\s)" + pattern + rf"[\s.!?,;:]*(?:{ARTIFACT}[\s.!?,;:]*){{0,2}}$",
                combined,
                re.I,
            )
            if match:
                return self.endings[phrase], match
        return None, None

    def final(self, transcript: str) -> Request | None:
        if self.finished:
            raise ValueError("This capture has already finished.")
        text = transcript.strip()
        combined = " ".join([*self.pieces, text]).strip()
        ending, match = self.ending(combined)
        if ending == Ending.CANCEL:
            self.pieces.clear()
            self.finished = True
            return Request(ending, "")
        if ending == Ending.SCRATCH:
            self.scratched = True
            before = combined[: match.start()].strip()
            remaining = len(before)
            kept = []
            for piece in [*self.pieces, text]:
                if remaining < len(piece):
                    break
                kept.append(piece)
                remaining -= len(piece) + 1
            if remaining <= 0 and kept:
                kept.pop()
            self.pieces = kept
            return None
        if ending is not None:
            combined = combined[: match.start()].strip()
            if len(combined) > 20_000:
                raise ValueError("This capture is full. Finish or cancel it before continuing.")
            self.pieces = [combined] if combined else []
            self.finished = True
            return Request(ending, self.text)
        if text:
            if len(combined) > 20_000:
                raise ValueError("This capture is full. Finish or cancel it before continuing.")
            self.pieces.append(text)
        return None


class Confirmation(Capture):
    def final(self, transcript: str) -> Request | None:
        phrase = " ".join(transcript.strip(" .!?,;:\t\r\n").casefold().split())
        if phrase in {"yes", "yes send it", "confirm"}:
            return Request(Ending.SUBMIT, "")
        if phrase in {"no", "never mind", "cancel"}:
            return Request(Ending.CANCEL, "")
        return None
