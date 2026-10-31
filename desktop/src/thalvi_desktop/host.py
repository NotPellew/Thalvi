"""Owns Linux window identity, focus, and literal text delivery through desktop tools."""

import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass

from .config import App, record


@dataclass(frozen=True)
class Target:
    address: str
    pid: int
    app: str


class DeliveryError(RuntimeError):
    """Delivery failed or may be partial; the workflow must preserve the captured text."""


def run(argv: Sequence[str], stdin: str | None = None) -> str:
    try:
        result = subprocess.run(
            argv, input=stdin, text=True, capture_output=True, timeout=8, check=True
        )
    except FileNotFoundError as error:
        raise RuntimeError(f"Desktop tool {argv[0]} is not installed.") from error
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(
            f"Desktop tool {argv[0]} took too long; check before trying again."
        ) from error
    except subprocess.CalledProcessError as error:
        raise RuntimeError(
            f"Desktop tool {argv[0]} failed; check the target before trying again."
        ) from error
    return result.stdout


def target(value: object) -> Target:
    item = record(value, "window")
    address, pid, app = item.get("address"), item.get("pid"), item.get("class")
    if not isinstance(address, str) or not re.fullmatch(r"0x[0-9a-fA-F]+", address):
        raise RuntimeError("The desktop did not identify the focused window.")
    if type(pid) is not int or pid <= 0 or not isinstance(app, str) or not app:
        raise RuntimeError("The desktop did not identify the focused application.")
    return Target(address, pid, app)


class Hyprland:
    def __init__(self, command: Callable[[Sequence[str], str | None], str] = run):
        self.command = command

    def capture(self) -> Target:
        return target(json.loads(self.command(["hyprctl", "-j", "activewindow"], None)))

    def windows(self) -> list[Target]:
        rows = json.loads(self.command(["hyprctl", "-j", "clients"], None))
        if not isinstance(rows, list):
            raise RuntimeError("The desktop returned an invalid window list.")
        windows = []
        for row in rows:
            item = record(row, "window")
            history = item.get("focusHistoryID", 2**31 - 1)
            if type(history) is not int:
                raise RuntimeError("The desktop returned invalid window history.")
            windows.append((history if history >= 0 else 2**31 - 1, target(item)))
        windows.sort(key=lambda item: item[0])
        return [window for _, window in windows]

    def copy(self, text: str) -> None:
        if text:
            self.command(["wl-copy", "--type", "text/plain;charset=utf-8"], text)

    def focus(self, selected: Target) -> None:
        if selected not in self.windows():
            raise DeliveryError("The original window closed. Nothing was typed.")
        self.command(["hyprctl", "dispatch", "focuswindow", f"address:{selected.address}"], None)
        if self.capture() != selected:
            raise DeliveryError("The target could not be focused. Nothing was typed.")

    def deliver(self, selected: Target, text: str, enter: bool) -> None:
        if any(ord(character) < 32 or 127 <= ord(character) < 160 for character in text):
            raise DeliveryError(
                "Captured text contains control characters that could press keys. "
                "Nothing was typed."
            )
        try:
            self.focus(selected)
            argv = ["wtype", "-"] + (["-k", "Return"] if enter else [])
            self.command(argv, text)
        except (RuntimeError, ValueError) as error:
            if isinstance(error, DeliveryError):
                raise
            raise DeliveryError(
                "Typing failed or stopped partway. Check the target before retrying."
            ) from error

    def focus_app(self, app: App) -> Target:
        matches = [
            window
            for window in self.windows()
            if window.app.casefold() in {name.casefold() for name in app.classes}
        ]
        if not matches:
            raise RuntimeError("No matching app window is open for this demo action.")
        selected = matches[0]
        self.focus(selected)
        if app.input_keys:
            self.command(["wtype", *key_arguments(app.input_keys)], None)
        return selected

    def open_url(self, url: str) -> None:
        self.command(["xdg-open", url], None)


def key_arguments(keys: tuple[str, ...]) -> list[str]:
    lowered = [key.casefold() for key in keys]
    result = [part for key in lowered[:-1] for part in ("-M", key)]
    return [*result, "-k", keys[-1]]


def desktop() -> Hyprland:
    if sys.platform != "linux" or not os.environ.get("HYPRLAND_INSTANCE_SIGNATURE"):
        raise RuntimeError(
            "This first desktop build needs Hyprland. Other desktop adapters are not qualified yet."
        )
    return Hyprland()
