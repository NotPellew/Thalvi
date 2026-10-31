"""Owns delivery and confirmation after a capture has chosen its route."""

import asyncio
import contextlib
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field

from .capture import Ending, Request
from .config import Config
from .discord import (
    AttemptUncertain,
    DesktopComposer,
    DiscordError,
    InsertionUncertain,
    PreviewComposer,
    validate_body,
)
from .host import Hyprland, Target
from .metrics import Metrics
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


class Workflow:
    def __init__(
        self,
        config: Config,
        host: Hyprland | PreviewHost,
        notify: Callable[[str], None] = notice,
        discord: PreviewComposer | DesktopComposer | None = None,
        metrics: Metrics | None = None,
    ):
        self.config, self.host, self.notify = config, host, notify
        self.metrics = metrics or Metrics(config.metrics)
        self.discord = discord if discord is not None else PreviewComposer()

    async def _preserve(self, body: str) -> None:
        started = time.monotonic()
        try:
            await asyncio.to_thread(self.host.copy, body)
        except (RuntimeError, OSError):
            with contextlib.suppress(OSError):
                await self.metrics.record("desktop_clipboard", started, "error")
            self.notify(f"Clipboard recovery failed. Your message follows:\n{body}")
        else:
            with contextlib.suppress(OSError):
                await self.metrics.record("desktop_clipboard", started, "ok")
            self.notify("The message is on the clipboard.")

    async def _discord_task(
        self, task: DiscordMessage, confirm: Callable[[], Awaitable[bool]]
    ) -> tuple[str, bool]:
        try:
            body = validate_body(task.message)
        except ValueError as error:
            self.notify(f"{error} Nothing was typed or sent.")
            return "failed", False
        try:
            prepared = await self.discord.prepare(task.destination)
        except (DiscordError, OSError, ValueError) as error:
            self.notify(f"{error} Nothing was typed or sent.")
            await self._preserve(task.message)
            return "failed", False
        try:
            try:
                await self.discord.enter_draft(prepared, body)
            except InsertionUncertain as error:
                self.notify(f"{error} Nothing was pressed.")
                await self._preserve(body)
                return "uncertain", False
            except asyncio.CancelledError:
                session = getattr(prepared, "session", None)
                if getattr(session, "insert_uncertain", False):
                    self.notify(
                        "Typing may have completed; the draft is unconfirmed. "
                        "Check Discord. Nothing was pressed or retried."
                    )
                    await self._preserve(body)
                elif getattr(session, "insert_sent", False):
                    self.notify(
                        "Typing was interrupted; nothing was pressed. The draft may remain. "
                        "Check Discord before sending."
                    )
                    await self._preserve(body)
                else:
                    self.notify("Cancelled before typing; nothing was typed.")
                    await self._preserve(body)
                raise
            except (DiscordError, OSError, ValueError) as error:
                self.notify(f"{error} Nothing was pressed; the draft may remain.")
                await self._preserve(body)
                return "failed", False
            self.notify(
                f"Send to {task.label}?\nDestination: {task.destination}\nMessage:\n{body}\n"
                "Say yes to send, or never mind to cancel."
            )
            if not await confirm():
                self.notify(
                    "Not sent. Nothing was pressed; the draft remains in Discord. "
                    "Check it before sending."
                )
                return "cancelled", True
            try:
                result = await self.discord.submit(prepared, body)
            except asyncio.CancelledError:
                if getattr(getattr(prepared, "session", None), "attempted", False):
                    self.notify(
                        "Submission may have completed; delivery is unconfirmed. "
                        "Check the channel before retrying; nothing was retried."
                    )
                    await self._preserve(body)
                raise
            except AttemptUncertain as error:
                self.notify(str(error))
                await self._preserve(body)
                return "uncertain", True
            except (DiscordError, OSError, ValueError) as error:
                self.notify(f"{error} The draft may remain; nothing was retried.")
                await self._preserve(body)
                return "failed", False
            self.notify(result)
            return "ok", True
        finally:
            try:
                await self.discord.close(prepared)
            except (DiscordError, OSError):
                self.notify("The Discord debug connection could not be closed.")

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
                if isinstance(task, DiscordMessage):
                    outcome, success = await self._discord_task(task, confirm)
                    return success
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
