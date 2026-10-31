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
    "submit": Ending.SUBMIT,
    "do that": Ending.TASK,
    "scratch that": Ending.SCRATCH,
    "never mind": Ending.CANCEL,
}


@dataclass(frozen=True)
class Request:
    ending: Ending
    text: str


@dataclass
class Capture:
    endings: dict[str, Ending] = field(default_factory=lambda: dict(DEFAULT_ENDINGS))
    pieces: list[str] = field(default_factory=list)
    finished: bool = False

    @property
    def text(self) -> str:
        return " ".join(self.pieces)

    def final(self, transcript: str) -> Request | None:
        if self.finished:
            raise ValueError("This capture has already finished.")
        text = transcript.strip()
        ending = None
        for phrase in sorted(self.endings, key=len, reverse=True):
            match = re.search(r"(?:^|\s)" + re.escape(phrase) + r"[\s.!?,;:]*$", text, re.I)
            if match:
                ending = self.endings[phrase]
                text = text[: match.start()].strip()
                break
        if ending == Ending.CANCEL:
            self.pieces.clear()
            self.finished = True
            return Request(ending, "")
        if ending == Ending.SCRATCH:
            if not text and self.pieces:
                self.pieces.pop()
            return None
        if text:
            if len(self.text) + len(text) + 1 > 20_000:
                raise ValueError("This capture is full. Finish or cancel it before continuing.")
            self.pieces.append(text)
        if ending is not None:
            self.finished = True
            return Request(ending, self.text)
        return None


class Confirmation(Capture):
    def final(self, transcript: str) -> Request | None:
        phrase = " ".join(transcript.strip(" .!?,;:\t\r\n").casefold().split())
        if phrase in {"yes", "yes send it", "confirm"}:
            return Request(Ending.SUBMIT, "")
        if phrase in {"no", "never mind", "cancel"}:
            return Request(Ending.CANCEL, "")
        return None
