"""Owns delivery and confirmation after a capture has chosen its route."""

import asyncio
import json
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from pathlib import Path

from .capture import Ending, Request
from .config import Config
from .host import Hyprland, Target
from .tasks import decide, execute


def notice(message: str) -> None:
    print(message, flush=True)


@dataclass
class PreviewHost:
    notices: Callable[[str], None] = notice
    operations: list[tuple[str, str, bool]] = field(default_factory=list)

    def capture(self) -> Target:
        return Target("0x1", 1, "preview target")

    def copy(self, text: str) -> None:
        self.operations.append(("clipboard", text, False))
        self.notices("Preview · text would be kept on the clipboard.")

    def deliver(self, selected: Target, text: str, enter: bool) -> None:
        self.operations.append(("deliver", text, enter))
        self.notices(
            f"Preview · deliver {len(text)} characters to {selected.app}"
            + (" and press Enter." if enter else ".")
        )

    def open_url(self, url: str) -> None:
        self.operations.append(("open_url", url, False))
        self.notices("Preview · request a browser tab.")

    def focus_app(self, app) -> Target:
        self.operations.append(("focus_app", app.classes[0], False))
        return self.capture()


class Metrics:
    def __init__(self, path: Path | None):
        self.path = path

    async def record(self, event: str, started: float, outcome: str) -> None:
        if self.path is None:
            return
        line = json.dumps(
            {
                "event": event,
                "total_ms": round((time.monotonic() - started) * 1000),
                "outcome": outcome,
            }
        )

        def write() -> None:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.path.open("a", encoding="utf-8") as stream:
                stream.write(line + "\n")

        await asyncio.to_thread(write)


class Workflow:
    def __init__(
        self, config: Config, host: Hyprland | PreviewHost, notify: Callable[[str], None] = notice
    ):
        self.config, self.host, self.notify = config, host, notify
        self.metrics = Metrics(config.metrics)

    async def finish(
        self, request: Request, selected: Target, confirm: Callable[[], Awaitable[bool]]
    ) -> bool:
        started = time.monotonic()
        outcome = "failed"
        event = "desktop_task" if request.ending == Ending.TASK else "desktop_delivery"
        try:
            if request.ending == Ending.CANCEL:
                self.notify("Discarded. Waiting for a wake word.")
                outcome = "cancelled"
                return True
            if request.ending == Ending.TASK:
                task = decide(request.text, self.config)
                self.notify("Working on the configured task.")
                result = await asyncio.to_thread(execute, task, self.config, self.host)
                self.notify(result)
            else:
                if request.ending == Ending.SUBMIT:
                    self.notify(
                        f"Submit to {selected.app}? This presses Enter. "
                        f"Say yes or never mind.\n{request.text}"
                    )
                    if not await confirm():
                        self.notify("Cancelled. Nothing was typed or submitted.")
                        outcome = "cancelled"
                        return True
                if request.text or request.ending == Ending.SUBMIT:
                    await asyncio.to_thread(
                        self.host.deliver, selected, request.text, request.ending == Ending.SUBMIT
                    )
                self.notify("Delivered. Waiting for a wake word.")
            outcome = "ok"
            return True
        except asyncio.CancelledError:
            outcome = "interrupted"
            raise
        except (RuntimeError, ValueError, OSError) as error:
            try:
                await asyncio.to_thread(self.host.copy, request.text)
                self.notify(f"{error} Captured text is on the clipboard.")
            except (RuntimeError, OSError):
                self.notify(
                    f"{error} Clipboard recovery failed. Your text follows:\n{request.text}"
                )
            return False
        finally:
            try:
                await self.metrics.record(event, started, outcome)
            except OSError:
                self.notify("The optional desktop metrics could not be saved.")
