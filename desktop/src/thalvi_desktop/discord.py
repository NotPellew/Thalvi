"""Owns the bounded Discord Desktop composer adapter and its one-shot submission."""

import asyncio
import contextlib
import os
import time
import unicodedata
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol
from urllib.parse import urlsplit

from . import cdp
from .cdp import CDP, CDPError, debug_port
from .cdp import Page as DebugPage
from .config import App, Config
from .host import Hyprland, Target
from .metrics import Metrics

DISCORD_ORIGIN = "https://discord.com"
BODY_LIMIT = 2000
POLL = 0.05
GUARD = Path(__file__).with_name("discord_dom.js").read_text(encoding="utf-8").strip()
GUARD_EXPR = GUARD.removesuffix(";")

REASONS = {
    "editor-missing": "The Discord message box was not found.",
    "editor-ambiguous": "Discord has more than one message box.",
    "editor-hidden": "The Discord message box is not visible.",
    "editor-label": "The focused editor is not Discord's message box.",
    "editor-unfocused": "The Discord message box is not focused.",
    "route-mismatch": "Discord is not on the configured channel.",
    "draft-not-empty": "The Discord message box already contains a draft.",
    "draft-mismatch": "The Discord draft changed after typing.",
    "structure": "The Discord composer has an unfamiliar structure.",
    "structure-changed": "The Discord composer changed after it was checked.",
    "control": "The Discord composer has an unfamiliar control.",
    "files": "The Discord composer has an attachment.",
    "menu-open": "A Discord menu is open.",
    "dialog": "A Discord dialog is open.",
    "text": "The Discord composer has an unexpected banner or label.",
}


class DiscordError(RuntimeError):
    """Discord's local debug surface was unavailable or the composer was refused."""


class DiscordUnavailable(DiscordError):
    """The verified local Discord process or debugger is not reachable."""


class DiscordRefused(DiscordError):
    """The bounded composer guard did not recognize a safe empty composer."""

    def __init__(self, reason: str | None):
        super().__init__(
            REASONS.get(reason or "", "The Discord composer is in an unfamiliar state.")
        )


class AttemptUncertain(DiscordError):
    """Enter may have reached Discord; the result must stay unconfirmed and unretried."""


def route_for(destination: str) -> str:
    parsed = urlsplit(destination)
    if (
        parsed.scheme != "https"
        or parsed.hostname != "discord.com"
        or parsed.port is not None
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
        or not _channel_path(parsed.path)
    ):
        raise ValueError("Discord destination must be a channel or DM URL.")
    return parsed.path


def _channel_path(path: str) -> bool:
    parts = path.split("/")
    return (
        len(parts) == 4
        and parts[0] == ""
        and parts[1] == "channels"
        and (parts[2].isdigit() or parts[2] == "@me")
        and parts[3].isdigit()
    )


def validate_body(text: str) -> str:
    if not text:
        raise ValueError("The Discord message is empty.")
    if len(text) > BODY_LIMIT:
        raise ValueError(f"The Discord message is longer than {BODY_LIMIT} characters.")
    for character in text:
        if unicodedata.category(character).startswith("C") or character in {"\u2028", "\u2029"}:
            raise ValueError("The Discord message contains unsupported control characters.")
    return text


@dataclass(frozen=True)
class Observation:
    ok: bool
    reason: str | None
    retryable: bool
    draft: str
    digest: str
    focused: bool
    route: str
    editor: str | None


def observation(value: object, editor: str | None) -> Observation:
    if not isinstance(value, dict):
        raise DiscordError("Discord returned an invalid composer observation.")
    ok, retryable = value.get("ok"), value.get("retryable")
    focused = value.get("focused")
    reason, draft, digest, route = (
        value.get("reason"),
        value.get("draft"),
        value.get("digest"),
        value.get("route"),
    )
    if (
        type(ok) is not bool
        or type(retryable) is not bool
        or type(focused) is not bool
        or (reason is not None and not isinstance(reason, str))
        or not isinstance(draft, str)
        or not isinstance(digest, str)
        or not isinstance(route, str)
    ):
        raise DiscordError("Discord returned an invalid composer observation.")
    return Observation(ok, reason, retryable, draft, digest, focused, route, editor)


@dataclass(frozen=True)
class Guarded:
    route: str
    editor: str
    digest: str


class ComposerPage(Protocol):
    async def route_state(self) -> tuple[str, str, str]: ...

    async def navigate(self, url: str) -> None: ...

    async def observe(self, editor: str | None) -> Observation: ...

    async def verify(self, editor: str, route: str, draft: str, digest: str) -> Observation: ...

    async def insert(self, editor: str, route: str, digest: str, text: str) -> Observation: ...

    async def enter(
        self, editor: str, route: str, digest: str, text: str, on_send: Callable[[], None]
    ) -> Observation: ...

    async def close(self) -> None: ...


def _nested_value(result: dict[str, object], label: str) -> object:
    remote = result.get("result")
    if not isinstance(remote, dict) or "value" not in remote:
        raise DiscordError(f"Discord returned no {label}.")
    return remote["value"]


class CDPPage:
    def __init__(self, cdp: CDP, route: str):
        self.cdp, self.route = cdp, route

    async def route_state(self) -> tuple[str, str, str]:
        result = await self.cdp.call(
            "Runtime.evaluate",
            {
                "expression": (
                    "({route: location.origin + location.pathname, "
                    "search: location.search, hash: location.hash})"
                ),
                "returnByValue": True,
            },
        )
        value = _nested_value(result, "route")
        if not isinstance(value, dict):
            raise DiscordError("Discord returned an invalid route.")
        route, search, fragment = value.get("route"), value.get("search"), value.get("hash")
        if not all(isinstance(part, str) for part in (route, search, fragment)):
            raise DiscordError("Discord returned an invalid route.")
        return route, search, fragment

    async def navigate(self, url: str) -> None:
        result = await self.cdp.call("Page.navigate", {"url": url})
        if result.get("errorText"):
            raise DiscordError("Discord refused to open the configured channel.")

    async def _editor(self) -> str | None:
        result = await self.cdp.call(
            "Runtime.evaluate",
            {"expression": f"({GUARD_EXPR}).findEditor()", "returnByValue": False},
        )
        remote = result.get("result")
        if not isinstance(remote, dict):
            raise DiscordError("Discord returned an invalid message box.")
        if remote.get("subtype") == "null" or "objectId" not in remote:
            return None
        object_id = remote.get("objectId")
        if not isinstance(object_id, str):
            raise DiscordError("Discord returned an invalid message box.")
        return object_id

    async def _call(
        self,
        editor: str,
        method: str,
        arguments: list[object],
        on_send: Callable[[], None] | None = None,
    ) -> Observation:
        function = (
            f"function(route, draft, digest, text) {{ const guard = ({GUARD_EXPR}); "
            f"return guard.{method}(this, route, draft, digest, text); }}"
        )
        try:
            result = await self.cdp.call(
                "Runtime.callFunctionOn",
                {
                    "objectId": editor,
                    "functionDeclaration": function,
                    "arguments": arguments,
                    "returnByValue": True,
                },
                **({"on_send": on_send} if on_send is not None else {}),
            )
        except CDPError as error:
            if error.code == -32000:
                return Observation(False, "editor-missing", False, "", "", False, "", editor)
            raise
        return observation(_nested_value(result, "composer observation"), editor)

    async def observe(self, editor: str | None) -> Observation:
        try:
            if editor is None:
                editor = await self._editor()
                if editor is None:
                    return Observation(False, "editor-missing", True, "", "", False, "", None)
        except CDPError as error:
            if error.code == -32000:
                return Observation(
                    False, "editor-missing", editor is None, "", "", False, "", editor
                )
            raise
        return await self._call(editor, "observe", [{"value": self.route}])

    async def verify(self, editor: str, route: str, draft: str, digest: str) -> Observation:
        return await self._call(
            editor,
            "verify",
            [{"value": route}, {"value": draft}, {"value": digest}],
        )

    async def insert(self, editor: str, route: str, digest: str, text: str) -> Observation:
        return await self._call(
            editor, "insert", [{"value": route}, {"value": ""}, {"value": digest}, {"value": text}]
        )

    async def enter(
        self, editor: str, route: str, digest: str, text: str, on_send: Callable[[], None]
    ) -> Observation:
        return await self._call(
            editor, "enter", [{"value": route}, {"value": text}, {"value": digest}], on_send
        )

    async def close(self) -> None:
        await self.cdp.close()


class ComposerSession:
    def __init__(self, page: ComposerPage, route: str, deadline: float = 5.0):
        self.page, self.route, self.deadline = page, route, deadline
        self.attempted = False

    async def prepare(self) -> Guarded:
        deadline = time.monotonic() + self.deadline
        while True:
            current = await self.page.observe(None)
            if current.ok and current.draft:
                raise DiscordRefused("draft-not-empty")
            if current.ok and current.focused:
                if current.editor is None:
                    raise DiscordError("Discord returned no message box.")
                return Guarded(self.route, current.editor, current.digest)
            reason = current.reason or "editor-unfocused"
            retryable = current.retryable or (current.ok and not current.focused)
            if not retryable or time.monotonic() >= deadline:
                raise DiscordRefused(reason)
            await asyncio.sleep(POLL)

    async def enter_draft(self, guarded: Guarded, body: str) -> None:
        current = await self.page.insert(guarded.editor, guarded.route, guarded.digest, body)
        if not current.ok:
            raise DiscordRefused(current.reason)
        deadline = time.monotonic() + self.deadline
        while True:
            current = await self.page.verify(guarded.editor, guarded.route, body, guarded.digest)
            if current.ok:
                return
            if not current.retryable or time.monotonic() >= deadline:
                raise DiscordRefused(current.reason)
            await asyncio.sleep(POLL)

    async def submit(self, guarded: Guarded, body: str) -> None:
        def mark_attempted() -> None:
            self.attempted = True

        try:
            current = await self.page.enter(
                guarded.editor, guarded.route, guarded.digest, body, mark_attempted
            )
            if not current.ok:
                self.attempted = False
                raise DiscordRefused(current.reason)
        except DiscordRefused:
            raise
        except (CDPError, DiscordError) as error:
            if not self.attempted:
                raise
            raise AttemptUncertain(
                "The Discord connection failed during submission. "
                "Submission state is unknown; delivery is unconfirmed. "
                "Check the channel; nothing was retried."
            ) from error

    async def close(self) -> None:
        await self.page.close()


@dataclass(frozen=True)
class PreviewGuarded:
    destination: str
    route: str


class PreviewComposer:
    def __init__(self):
        self.operations: list[tuple[str, ...]] = []

    async def prepare(self, destination: str) -> PreviewGuarded:
        return PreviewGuarded(destination, route_for(destination))

    async def enter_draft(self, guarded: PreviewGuarded, body: str) -> None:
        return None

    async def submit(self, guarded: PreviewGuarded, body: str) -> str:
        self.operations.append(("enter",))
        return (
            "Preview · would insert the message and press Enter once. Delivery stays unconfirmed."
        )

    async def close(self, guarded: PreviewGuarded) -> None:
        return None


@dataclass(frozen=True)
class DiscordProcess:
    window: Target
    owner: int
    executable: Path


def _exe(pid: int, root: Path) -> Path:
    try:
        target = os.readlink(root / str(pid) / "exe")
    except OSError as error:
        raise DiscordUnavailable("The Discord process is not visible.") from error
    if target.endswith(" (deleted)"):
        target = target.removesuffix(" (deleted)")
    return Path(os.path.normpath(target))


def _uid(pid: int, root: Path) -> int:
    try:
        lines = (root / str(pid) / "status").read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise DiscordUnavailable("The Discord process is not visible.") from error
    for line in lines:
        if line.startswith("Uid:"):
            fields = line.split()
            if len(fields) > 1 and fields[1].isdigit():
                return int(fields[1])
    raise DiscordUnavailable("The Discord process has no visible owner.")


def _listener_inode(port: int, root: Path) -> str:
    debug_port(port)
    inodes = []
    for name in ("tcp", "tcp6"):
        try:
            lines = (root / "net" / name).read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for line in lines[1:]:
            fields = line.split()
            if len(fields) < 10 or fields[3] != "0A" or ":" not in fields[1]:
                continue
            address, port_hex = fields[1].split(":", 1)
            try:
                number = int(port_hex, 16)
            except ValueError:
                continue
            if number != port:
                continue
            if address != "0100007F":
                raise DiscordUnavailable("The Discord debug port is not bound to 127.0.0.1.")
            inodes.append(fields[9])
    if not inodes or not inodes[0].isdigit():
        raise DiscordUnavailable("Discord's local debug port is not open.")
    return inodes[0]


def _owner(inode: str, root: Path) -> int:
    socket = f"socket:[{inode}]"
    try:
        entries = sorted(root.iterdir(), key=lambda entry: entry.name)
    except OSError as error:
        raise DiscordUnavailable("The process table is not readable.") from error
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            descriptors = list((entry / "fd").iterdir())
        except OSError:
            continue
        for descriptor in descriptors:
            try:
                target = os.readlink(descriptor)
            except OSError:
                continue
            if target == socket:
                return int(entry.name)
    raise DiscordUnavailable("The Discord debug port has no visible owner.")


def verify_process(
    port: int,
    windows: list[Target],
    classes: tuple[str, ...],
    root: Path = Path("/proc"),
    uid: int | None = None,
) -> DiscordProcess:
    names = {name.casefold() for name in classes}
    matches = [window for window in windows if window.app.casefold() in names]
    if not matches:
        raise DiscordUnavailable("No Discord Desktop window is open.")
    window = matches[0]
    executable = _exe(window.pid, root)
    if executable.name != "Discord" or not executable.parent.name.startswith("app-"):
        raise DiscordUnavailable("The Discord window is not the Discord application.")
    if _uid(window.pid, root) != (os.getuid() if uid is None else uid):
        raise DiscordUnavailable("The Discord window belongs to another user.")
    owner = _owner(_listener_inode(port, root), root)
    if _uid(owner, root) != (os.getuid() if uid is None else uid):
        raise DiscordUnavailable("The Discord debug port belongs to another user.")
    if _exe(owner, root) != executable:
        raise DiscordUnavailable("The debug port is not owned by the Discord window's process.")
    return DiscordProcess(window, owner, executable)


@dataclass
class LiveGuarded:
    route: str
    session: ComposerSession
    guarded: Guarded


class DesktopComposer:
    def __init__(
        self,
        config: Config,
        metrics: Metrics,
        *,
        windows: Callable[[], list[Target]] | None = None,
        root: Path = Path("/proc"),
        deadline: float = 5.0,
        timeout: float = 3.0,
    ):
        self.config, self.metrics = config, metrics
        self.port = config.discord_port
        self.windows = windows or Hyprland().windows
        self.root, self.deadline, self.timeout = root, deadline, timeout
        self.classes = _classes(config)

    async def _record(self, event: str, started: float, outcome: str) -> None:
        with contextlib.suppress(OSError):
            await self.metrics.record(event, started, outcome)

    async def prepare(self, destination: str) -> LiveGuarded:
        full = DISCORD_ORIGIN + route_for(destination)
        started = time.monotonic()
        try:
            windows = await asyncio.to_thread(self.windows)
            await self._record("discord_window", started, "ok")
        except (RuntimeError, OSError, ValueError) as error:
            await self._record("discord_window", started, "error")
            raise DiscordUnavailable("The Discord window list is unavailable.") from error
        started = time.monotonic()
        try:
            verify_process(self.port, windows, self.classes, self.root)
            await self._record("discord_listener", started, "ok")
        except DiscordError:
            await self._record("discord_listener", started, "error")
            raise
        _, targets = await cdp.discover(self.port, self.metrics, self.timeout)
        page: DebugPage = cdp.select_page(self.port, targets)
        session = await CDP(page.websocket, self.metrics, self.timeout).open()
        try:
            adapter = CDPPage(session, full)
            route, search, fragment = await adapter.route_state()
            if route != full or search or fragment:
                await adapter.navigate(destination)
            composer = ComposerSession(adapter, full, self.deadline)
            guarded = await composer.prepare()
            return LiveGuarded(full, composer, guarded)
        except BaseException:
            await session.close()
            raise

    async def enter_draft(self, prepared: LiveGuarded, body: str) -> None:
        await prepared.session.enter_draft(prepared.guarded, body)

    async def submit(self, prepared: LiveGuarded, body: str) -> str:
        await prepared.session.submit(prepared.guarded, body)
        return "Submission attempted; delivery unconfirmed."

    async def close(self, prepared: LiveGuarded) -> None:
        await prepared.session.close()


def _classes(config: Config) -> tuple[str, ...]:
    app: App | None = config.apps.get("discord")
    return app.classes if app else ("discord", "vesktop")
