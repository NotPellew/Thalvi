"""Owns delivery and confirmation after a capture has chosen its route."""

import asyncio
import json
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from pathlib import Path

from .capture import Ending, Request
from .config import Config
from .discord import Discord
from .host import Hyprland, Target
from .tasks import DiscordMessage, decide, execute


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

    def write(self, event: str, duration: float, outcome: str) -> None:
        if self.path is None:
            return
        line = json.dumps({"event": event, "total_ms": round(duration * 1000), "outcome": outcome})
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write(line + "\n")

    async def record(self, event: str, started: float, outcome: str) -> None:
        await asyncio.to_thread(self.write, event, time.monotonic() - started, outcome)


class Workflow:
    def __init__(
        self,
        config: Config,
        host: Hyprland | PreviewHost,
        notify: Callable[[str], None] = notice,
        discord: Discord | None = None,
        preview: bool = False,
    ):
        self.config, self.host, self.notify = config, host, notify
        self.discord = discord or Discord()
        self.preview = preview
        self.metrics = Metrics(config.metrics)

    def _record_call(self, event: str, started: float, outcome: str) -> None:
        duration = time.monotonic() - started
        try:
            self.metrics.write(event, duration, outcome)
        except OSError:
            self.notify("The optional desktop metrics could not be saved.")

    async def _endpoint(self, operation, webhook, task) -> None:
        await asyncio.to_thread(operation, webhook, task, self._record_call)

    async def finish(
        self, request: Request, selected: Target, confirm: Callable[[], Awaitable[bool]]
    ) -> bool:
        started = time.monotonic()
        outcome = "failed"
        event = "desktop_task" if request.ending == Ending.TASK else "desktop_delivery"
        discord_phase = None
        try:
            if request.ending == Ending.CANCEL:
                self.notify("Discarded. Waiting for a wake word.")
                outcome = "cancelled"
                return True
            if request.ending == Ending.TASK:
                task = decide(request.text, self.config)
                if isinstance(task, DiscordMessage):
                    if self.preview:
                        self.discord.validate(task)
                        webhook = None
                    else:
                        webhook = await asyncio.to_thread(self.discord.prepare, task)
                    self.notify(
                        f"Send this message to {task.label}?\n{task.message}\n"
                        "Say yes to send, or never mind to cancel."
                    )
                    if not await confirm():
                        self.notify("Cancelled. Nothing was sent.")
                        outcome = "cancelled"
                        return True
                    if webhook is None:
                        self.notify(f"Preview · would send to {task.label}; nothing was sent.")
                    else:
                        discord_phase = "check"
                        await self._endpoint(self.discord.check, webhook, task)
                        discord_phase = "send"
                        await self._endpoint(self.discord.send, webhook, task)
                        discord_phase = None
                        self.notify(f"Sent to {task.label}.")
                else:
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
            if discord_phase is not None:
                message = (
                    "The Discord send may have completed. Check the channel before retrying."
                    if discord_phase == "send"
                    else "Discord destination check cancelled. Nothing was sent."
                )
                try:
                    await asyncio.to_thread(self.host.copy, request.text)
                    self.notify(f"{message} Captured text is on the clipboard.")
                except (RuntimeError, OSError):
                    self.notify(
                        f"{message} Clipboard recovery failed. Your text follows:\n{request.text}"
                    )
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
