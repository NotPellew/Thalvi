"""Owns provisional endings until a continuous quiet interval has passed."""

from copy import deepcopy
from dataclasses import replace

from .capture import Capture, Ending, Request


class Pending:
    def __init__(self, capture: Capture, seconds: float):
        self.capture = capture
        self.seconds = seconds
        self.request: Request | None = None
        self.since = 0.0

    def continuation(self) -> None:
        self.request = None

    def final(self, text: str, now: float) -> Request | None:
        if not text.strip():
            return None
        self.continuation()
        trial = deepcopy(self.capture)
        request = trial.final(text)
        if request is None or request.ending == Ending.CANCEL:
            self.capture.pieces = trial.pieces
            self.capture.finished = trial.finished
            return request
        combined = " ".join([*self.capture.pieces, text.strip()]).strip()
        if len(combined) > 20_000:
            raise ValueError("This capture is full. Finish or cancel it before continuing.")
        self.capture.pieces.append(text.strip())
        self.request = request
        self.since = now
        return None

    def ready(self, now: float) -> Request | None:
        if self.request is None or now - self.since < self.seconds:
            return None
        request = replace(self.request, pause_confirmed=self.request.ending == Ending.SUBMIT)
        self.request = None
        self.capture.pieces = [request.text] if request.text else []
        self.capture.finished = True
        return request
