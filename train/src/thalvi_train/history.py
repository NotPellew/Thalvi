"""Earlier text turns for training and evaluation prompts, drawn from other records.

In the live product the turns before a spoken one are text. A projector trained on
single-turn prompts only learned to echo the request in front of it, and with text
history in context it can copy an earlier user turn instead of hearing the audio.
Giving some samples plausible earlier turns teaches it to listen anyway.
"""

import random
import re
from bisect import bisect_left
from collections.abc import Sequence
from dataclasses import dataclass

from thalvi_train.prompts import History, Turn
from thalvi_train.records import Record, Task

MAX_ANSWER_CHARS = 400
_ANSWER = re.compile(r"to=user<\|message\|>(.*?)(?:<\|eot\|>|\Z)", re.DOTALL)


def answer_of(response: str) -> str:
    """The spoken answer of a rendered `response` or generation, no reasoning; empty if absent.

    Tolerates a text cut off anywhere, since a student may run out of tokens.
    """
    match = _ANSWER.search(response)
    return match.group(1) if match else ""


def history_chars(history: History) -> int:
    """Characters in `history`, to weigh a sample's length."""
    return sum(len(user) + len(assistant) for user, assistant in history)


@dataclass(frozen=True)
class HistoryConfig:
    """How often a sample gets earlier turns, and which.

    With probability `prob` a sample gets 1 to `turns` earlier turns of other records.
    Each draw comes from records of the sample's own task with probability
    `same_task`, so the history holds plausible confusers.
    """

    prob: float = 0.5
    turns: int = 3
    same_task: float = 0.5

    def __post_init__(self) -> None:
        if not (0 <= self.prob <= 1 and 0 <= self.same_task <= 1):
            raise ValueError("history probabilities must be between 0 and 1")
        if self.turns < 1:
            raise ValueError("history turns must be at least 1")


class HistorySampler:
    """Draws reproducible histories for the records of one run.

    A history turn is another record's `text` as the user and the answer part of its
    response, cut to `MAX_ANSWER_CHARS`, as the assistant. Records with no answer are
    never drawn, and neither is the sample's own record.
    """

    def __init__(self, records: Sequence[Record], config: HistoryConfig, seed: int) -> None:
        self._config = config
        self._seed = seed
        self._tasks: list[Task] = [record.task for record in records]
        self._turns: list[Turn] = []
        self._all: list[int] = []
        self._by_task: dict[Task, list[int]] = {}
        for index, record in enumerate(records):
            answer = answer_of(record.response)[:MAX_ANSWER_CHARS].rstrip()
            self._turns.append((record.text, answer))
            if answer:
                self._all.append(index)
                self._by_task.setdefault(record.task, []).append(index)

    def draw(self, index: int, epoch: int = 0) -> History:
        """The history in front of record `index` in `epoch`; empty when none is drawn.

        Seeded by the run seed, `epoch` and `index` alone, so a rerun draws the same.
        """
        rng = random.Random(f"{self._seed}:{epoch}:{index}")
        if rng.random() >= self._config.prob:
            return ()
        count = rng.randint(1, self._config.turns)
        chosen: list[int] = []
        if rng.random() < self._config.same_task:
            chosen = self._pick(self._by_task.get(self._tasks[index], []), index, count, rng)
        chosen = chosen or self._pick(self._all, index, count, rng)
        return tuple(self._turns[j] for j in chosen)

    def draw_all(self, epoch: int = 0) -> list[History]:
        """The history of every record in `epoch`, in record order."""
        return [self.draw(index, epoch) for index in range(len(self._tasks))]

    @staticmethod
    def _pick(pool: list[int], index: int, count: int, rng: random.Random) -> list[int]:
        """Up to `count` distinct members of the ascending `pool`, never `index` itself."""
        own = bisect_left(pool, index)
        present = own < len(pool) and pool[own] == index
        size = len(pool) - present
        return [pool[p + (present and p >= own)] for p in rng.sample(range(size), min(count, size))]
