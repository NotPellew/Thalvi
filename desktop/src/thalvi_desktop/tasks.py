"""Owns scripted decisions; execution accepts the same typed action from a future model."""

import re
from dataclasses import dataclass
from urllib.parse import urlsplit

from .config import Config
from .host import Hyprland


@dataclass(frozen=True)
class OpenUrl:
    url: str
    label: str


@dataclass(frozen=True)
class FocusApp:
    app: str
    label: str


@dataclass(frozen=True)
class DiscordMessage:
    destination: str
    message: str
    label: str


Task = OpenUrl | FocusApp | DiscordMessage


def decide(text: str, config: Config) -> Task:
    phrase = " ".join(text.casefold().strip(" .!?\t\r\n").split())
    for script in config.scripts:
        if script.action == "discord_message":
            prefix = script.phrase + " "
            if text.casefold().startswith(prefix) and text[len(prefix) :].strip():
                return DiscordMessage(script.destination, text[len(prefix) :].strip(), script.label)
            continue
        if phrase == script.phrase:
            if script.action == "open_url":
                return OpenUrl(script.destination, script.label)
            return FocusApp(script.destination, script.label)
    match = re.fullmatch(r"open\s+(\S+)", text.strip(), re.IGNORECASE)
    if match:
        value = match.group(1)
        candidate = value if "://" in value else "https://" + value
        try:
            parsed = urlsplit(candidate)
            hostname = parsed.hostname
            port = parsed.port
        except ValueError:
            hostname = None
            port = None
        if (
            hostname
            and parsed.scheme in {"http", "https"}
            and not parsed.username
            and not parsed.password
            and re.fullmatch(r"[A-Za-z0-9.-]+", hostname)
            and all(
                re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?", part)
                for part in hostname.split(".")
            )
            and "." in hostname
            and not re.search(r"[\s\x00-\x1f\x7f-\x9f\\]", candidate)
            and (port is None or port > 0)
            and not re.search(r"%(?![0-9A-Fa-f]{2})", candidate)
        ):
            return OpenUrl(candidate, value)
    raise ValueError("That task isn't configured yet. Your captured text is kept.")


def execute(task: Task, config: Config, host: Hyprland) -> str:
    match task:
        case DiscordMessage():
            raise RuntimeError("Discord messages run through the verified Discord composer.")
        case OpenUrl(url, label):
            host.open_url(url)
            return f"Asked the browser to open {label}."
        case FocusApp(app, label):
            host.focus_app(config.apps[app])
            return f"Focused {label}."
