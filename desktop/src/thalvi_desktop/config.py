"""Owns validation of desktop configuration and scripted task definitions."""

import json
import math
import re
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

from .capture import DEFAULT_ENDINGS, Ending


def record(value: object, label: str) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(k, str) for k in value):
        raise ValueError(f"{label} must be an object.")
    return value


def text(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be nonempty text.")
    return value.strip()


def fields(value: dict[str, object], allowed: set[str], label: str) -> None:
    if unknown := value.keys() - allowed:
        raise ValueError(f"Unknown {label} setting: {', '.join(sorted(unknown))}.")


def url(value: object, label: str, schemes: set[str]) -> str:
    result = text(value, label)
    parsed = urlsplit(result)
    if parsed.scheme not in schemes or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError(f"{label} must have a host, no credentials, and one of {sorted(schemes)}.")
    return result


@dataclass(frozen=True)
class App:
    classes: tuple[str, ...]
    input_keys: tuple[str, ...]


@dataclass(frozen=True)
class Script:
    phrase: str
    action: str
    destination: str
    label: str


@dataclass(frozen=True)
class Config:
    server: str
    capture_model: Path | None
    conversation_model: Path | None
    model_dir: Path | None
    threshold: float
    endings: dict[str, Ending]
    apps: dict[str, App]
    scripts: tuple[Script, ...]
    metrics: Path | None
    device: str | int | None

    @classmethod
    def load(cls, path: Path) -> "Config":
        raw = record(json.loads(path.read_text()), "Desktop config")
        fields(
            raw, {"server", "wake", "endings", "apps", "scripts", "metrics", "device"}, "desktop"
        )
        server = url(raw.get("server", "ws://127.0.0.1:8080"), "server", {"ws", "wss"})
        parsed = urlsplit(server)
        if parsed.path not in ("", "/") or parsed.query or parsed.fragment:
            raise ValueError("server must be the server root, without a path, query, or fragment.")
        wake = record(raw.get("wake", {}), "wake")
        fields(wake, {"capture_model", "conversation_model", "model_dir", "threshold"}, "wake")
        threshold = wake.get("threshold", 0.65)
        if (
            type(threshold) not in (int, float)
            or not math.isfinite(threshold)
            or not 0 < threshold < 1
        ):
            raise ValueError("wake.threshold must be between zero and one.")
        endings_raw = record(raw.get("endings", dict(DEFAULT_ENDINGS)), "endings")
        endings: dict[str, Ending] = {}
        for phrase, action in endings_raw.items():
            phrase = " ".join(text(phrase, "ending phrase").casefold().split())
            if phrase in endings:
                raise ValueError("Ending phrases must be distinct.")
            endings[phrase] = Ending(action)
        if set(endings.values()) != set(Ending):
            raise ValueError("Configure paste, submit, task, scratch, and cancel endings.")
        apps: dict[str, App] = {}
        for name, value in record(raw.get("apps", {}), "apps").items():
            item = record(value, f"apps.{name}")
            fields(item, {"classes", "input_keys"}, f"apps.{name}")
            classes, keys = item.get("classes"), item.get("input_keys", [])
            if not isinstance(classes, list) or not classes or not isinstance(keys, list):
                raise ValueError(f"apps.{name} needs classes and optional input_keys lists.")
            apps[name] = App(
                tuple(text(c, "window class") for c in classes), tuple(text(k, "key") for k in keys)
            )
            shortcut = apps[name].input_keys
            if shortcut and (
                any(key.casefold() not in {"ctrl", "shift", "alt", "logo"} for key in shortcut[:-1])
                or re.fullmatch(r"[A-Za-z0-9_]+", shortcut[-1]) is None
            ):
                raise ValueError("Input keys must be modifiers followed by one named key.")
        definitions = raw.get("scripts", [])
        if not isinstance(definitions, list):
            raise ValueError("scripts must be a list.")
        scripts: list[Script] = []
        for definition in definitions:
            item = record(definition, "script")
            fields(item, {"phrase", "action", "destination", "label"}, "script")
            phrase = " ".join(text(item.get("phrase"), "script phrase").casefold().split())
            action = item.get("action")
            destination = text(item.get("destination"), "script destination")
            if action == "open_url":
                destination = url(destination, "script URL", {"http", "https"})
            elif action == "discord_message":
                if (
                    destination
                    != "discord://discord.com/channels/1566030981687677039/1566034064333340732"
                ):
                    raise ValueError("Discord destination must be the configured demo channel.")
            elif action == "focus_app":
                if destination not in apps:
                    raise ValueError(f"No app named {destination!r} is configured.")
            else:
                raise ValueError("Script action must be open_url, focus_app or discord_message.")
            if any(s.phrase == phrase for s in scripts):
                raise ValueError("Script phrases must be distinct.")
            scripts.append(
                Script(phrase, action, destination, text(item.get("label", phrase), "script label"))
            )
        device = raw.get("device")
        if device is not None and (isinstance(device, bool) or not isinstance(device, (str, int))):
            raise ValueError("device must be a device name or index.")

        def local(value: object) -> Path | None:
            if value is None:
                return None
            result = Path(text(value, "local path")).expanduser()
            return result if result.is_absolute() else path.parent / result

        capture_model = local(wake.get("capture_model"))
        conversation_model = local(wake.get("conversation_model"))
        if capture_model is not None and capture_model == conversation_model:
            raise ValueError("Capture and conversation need different wake models.")
        return cls(
            server.rstrip("/"),
            capture_model,
            conversation_model,
            local(wake.get("model_dir")),
            float(threshold),
            endings,
            apps,
            tuple(scripts),
            local(raw.get("metrics")),
            device,
        )
