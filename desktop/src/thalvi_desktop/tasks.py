"""Owns scripted decisions; execution accepts the same typed action from a future model."""

from dataclasses import dataclass

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
