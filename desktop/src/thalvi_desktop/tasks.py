"""Owns scripted decisions; execution accepts the same typed action from a future model."""

from dataclasses import dataclass

from .config import (
    Config,
    DiscordScript,
    FocusAppScript,
    OpenUrlScript,
    WebhookSource,
)
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
    guild: str
    channel: str
    label: str
    message: str
    webhook: WebhookSource


Task = OpenUrl | FocusApp | DiscordMessage


def decide(text: str, config: Config) -> Task:
    phrase = " ".join(text.casefold().strip(" .!?\t\r\n").split())
    for script in config.scripts:
        if isinstance(script, DiscordScript):
            prefix = script.phrase + " "
            if text.casefold().startswith(prefix) and text[len(prefix) :].strip():
                return DiscordMessage(
                    script.guild,
                    script.channel,
                    script.label,
                    text[len(prefix) :].strip(),
                    script.webhook,
                )
            continue
        if phrase == script.phrase:
            if isinstance(script, OpenUrlScript):
                return OpenUrl(script.url, script.label)
            if isinstance(script, FocusAppScript):
                return FocusApp(script.app, script.label)
    raise ValueError("That task isn't configured yet. Your captured text is kept.")


def execute(task: OpenUrl | FocusApp, config: Config, host: Hyprland) -> str:
    match task:
        case OpenUrl(url, label):
            host.open_url(url)
            return f"Asked the browser to open {label}."
        case FocusApp(app, label):
            host.focus_app(config.apps[app])
            return f"Focused {label}."
