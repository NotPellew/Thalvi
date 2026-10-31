"""Exercises exact window targeting and literal input through the process boundary."""

import asyncio
import json
from pathlib import Path

import pytest

from thalvi_desktop.capture import Ending, Request
from thalvi_desktop.config import App, Config
from thalvi_desktop.host import Hyprland, Target
from thalvi_desktop.workflow import Workflow


def test_delivery_returns_to_original_window_and_text_never_becomes_a_shell_command():
    original = {"address": "0x123", "pid": 123, "class": "terminal"}
    active = {"address": "0x456", "pid": 456, "class": "browser"}
    calls = []

    def command(argv, stdin):
        nonlocal active
        calls.append((list(argv), stdin))
        if argv[-1] == "clients":
            return json.dumps([original, active])
        if argv[-1] == "activewindow":
            return json.dumps(active)
        if "focuswindow" in argv:
            active = original
        return ""

    host = Hyprland(command)
    selected = Target("0x123", 123, "terminal")
    text = "--help $(not-a-command) `also-not-a-command` Unicode café"
    host.deliver(selected, text, True)
    assert calls[-1] == (["wtype", "-", "-k", "Return"], text)
    assert active == original


@pytest.mark.parametrize("control", ["\n", "\r", "\t", "\x1b", "\0", "\b", "\x7f", "\x85"])
def test_control_text_never_types_or_presses_keys_and_is_preserved(control):
    from thalvi_desktop.capture import Capture

    calls = []
    row = {"address": "0x123", "pid": 123, "class": "editor"}

    def command(argv, stdin):
        calls.append((list(argv), stdin))
        if argv[-1] == "clients":
            return json.dumps([row])
        if argv[-1] == "activewindow":
            return json.dumps(row)
        return ""

    async def behavior():
        config = Config.load(Path(__file__).parents[1] / "config.example.json")
        request = Capture(config.endings).final(f"hello{control}world paste that")

        async def confirm():
            raise AssertionError("Paste does not request Enter.")

        return await Workflow(config, Hyprland(command), lambda _: None).finish(
            request, Target("0x123", 123, "editor"), confirm
        )

    assert asyncio.run(behavior()) is False
    assert calls == [(["wl-copy", "--type", "text/plain;charset=utf-8"], f"hello{control}world")]


@pytest.mark.parametrize("lost", ["closed", "reused", "focus_failed", "typing_failed"])
def test_failed_targeting_never_types_into_another_window_and_keeps_a_clipboard_copy(lost):
    original = {"address": "0x123", "pid": 123, "class": "editor"}
    other = {"address": "0x456", "pid": 456, "class": "browser"}
    calls = []

    def command(argv, stdin):
        calls.append((list(argv), stdin))
        if argv[-1] == "clients":
            return json.dumps(
                []
                if lost == "closed"
                else [{**original, "pid": 999}]
                if lost == "reused"
                else [original]
            )
        if argv[-1] == "activewindow":
            return json.dumps(other if lost == "focus_failed" else original)
        if argv[0] == "wtype":
            raise RuntimeError("keyboard failed")
        return ""

    async def behavior():
        config = Config.load(Path(__file__).parents[1] / "config.example.json")

        async def confirm():
            return True

        return await Workflow(config, Hyprland(command), lambda _: None).finish(
            Request(Ending.SUBMIT, "Keep this"),
            Target("0x123", 123, "editor"),
            confirm,
        )

    assert asyncio.run(behavior()) is False
    typed = [call for call in calls if call[0][0] == "wtype"]
    assert len(typed) == (1 if lost == "typing_failed" else 0)
    assert calls[-1] == (["wl-copy", "--type", "text/plain;charset=utf-8"], "Keep this")
    assert sum(call[0][0] == "wl-copy" for call in calls) == 1


def test_configured_input_shortcut_is_sent_only_after_focus_is_verified():
    row = {"address": "0x1", "pid": 1, "class": "firefox"}
    calls = []

    def command(argv, stdin):
        calls.append(list(argv))
        return (
            json.dumps([row] if argv[-1] == "clients" else row)
            if argv[-1] in {"clients", "activewindow"}
            else ""
        )

    Hyprland(command).focus_app(App(("firefox",), ("ctrl", "l")))
    assert calls[-1] == ["wtype", "-M", "ctrl", "-k", "l"]


@pytest.mark.parametrize(
    "browser, expected",
    [
        ("firefox_firefox.desktop", "/snap/bin/firefox"),
        ("firefox.desktop", "firefox"),
        ("chromium_chromium.desktop", "/snap/bin/chromium"),
        ("chromium.desktop", "chromium"),
        ("google-chrome.desktop", "google-chrome"),
        ("google-chrome-stable.desktop", "google-chrome-stable"),
    ],
)
def test_open_url_requests_new_tab_in_supported_default_browser(browser, expected):
    calls = []

    def command(argv, stdin):
        calls.append(list(argv))
        return browser if argv[0] == "xdg-settings" else ""

    Hyprland(command).open_url("https://example.com/Case?q=Value")
    assert calls == [
        ["xdg-settings", "get", "default-web-browser"],
        [expected, "--new-tab", "https://example.com/Case?q=Value"],
    ]


def test_open_url_rejects_unsupported_default_browser():
    calls = []

    def command(argv, stdin):
        calls.append(list(argv))
        return "other.desktop"

    with pytest.raises(RuntimeError, match="default browser"):
        Hyprland(command).open_url("https://example.com")
    assert calls == [["xdg-settings", "get", "default-web-browser"]]
