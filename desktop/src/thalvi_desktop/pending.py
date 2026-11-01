"""Owns provisional endings until a continuous quiet interval has passed."""

from copy import deepcopy
from dataclasses import replace

from .capture import Capture, Ending, Request, trailing_artifacts


class Pending:
    def __init__(self, capture: Capture, seconds: float):
        self.capture = capture
        self.seconds = seconds
        self.request: Request | None = None
        self.since = 0.0
        self.waiting_for_quiet = False

    def continuation(self) -> None:
        self.request = None

    def final(self, text: str, now: float, *, speech: bool = False) -> Request | None:
        if not text.strip() or trailing_artifacts(text):
            return None
        previous = self.request
        trial = deepcopy(self.capture)
        request = trial.final(text)
        if request is None or request.ending == Ending.CANCEL:
            self.request = None
            self.capture.pieces = trial.pieces
            self.capture.finished = trial.finished
            self.capture.scratched = trial.scratched
            return request
        combined = " ".join([*self.capture.pieces, text.strip()]).strip()
        if len(combined) > 20_000:
            raise ValueError("This capture is full. Finish or cancel it before continuing.")
        self.capture.pieces.append(text.strip())
        self.request = request
        if previous is None or request.ending != previous.ending or request.text != previous.text:
            self.since = now
            self.waiting_for_quiet = speech
        return None

    def ready(self, now: float) -> Request | None:
        if self.request is None or self.waiting_for_quiet or now - self.since < self.seconds:
            return None
        request = replace(self.request, pause_confirmed=self.request.ending == Ending.SUBMIT)
        self.request = None
        self.capture.pieces = [request.text] if request.text else []
        self.capture.finished = True
        return request
