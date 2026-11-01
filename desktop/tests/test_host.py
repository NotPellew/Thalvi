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


def test_cancelled_runner_never_starts_a_later_command(monkeypatch):
    from thalvi_desktop.host import CommandRunner

    runner = CommandRunner()
    runner.cancel()

    def unexpected(*_args, **_kwargs):
        raise AssertionError("cancelled command started")

    monkeypatch.setattr("thalvi_desktop.host.subprocess.Popen", unexpected)
    with pytest.raises(RuntimeError, match="cancelled"):
        runner(["wtype", "-"], "private text")


@pytest.mark.parametrize("stage", ["spawn", "before_feed", "after_feed"])
def test_cancel_kills_owned_child_and_blocks_later_input(monkeypatch, stage):
    import threading
    from concurrent.futures import ThreadPoolExecutor

    from thalvi_desktop.host import CommandRunner

    entered = threading.Event()
    release = threading.Event()
    killed = threading.Event()
    reaped = threading.Event()
    consumed = []
    runner = CommandRunner()

    class Child:
        returncode = None

        def poll(self):
            return self.returncode

        def kill(self):
            self.returncode = -9
            killed.set()

        def wait(self):
            assert killed.wait(2)
            reaped.set()
            return self.returncode

        def communicate(self, input=None, timeout=None):
            assert timeout == 8
            if stage == "before_feed":
                entered.set()
                assert release.wait(2)
            if not killed.is_set() and input is not None:
                consumed.append(input)
            if stage == "after_feed":
                entered.set()
                assert release.wait(2)
            assert killed.is_set()
            return "", ""

    def popen(*_args, **_kwargs):
        if stage == "spawn":
            entered.set()
            assert release.wait(2)
        return Child()

    monkeypatch.setattr("thalvi_desktop.host.subprocess.Popen", popen)
    with ThreadPoolExecutor(max_workers=2) as pool:
        delivery = pool.submit(runner, ["fake-input"], "body")
        assert entered.wait(2)
        cancellation = pool.submit(runner.cancel)
        assert runner.cancelled.wait(2)
        if stage == "spawn":
            release.set()
        assert killed.wait(2)
        if stage != "spawn":
            release.set()
        cancellation.result(timeout=2)
        with pytest.raises(RuntimeError, match="cancelled"):
            delivery.result(timeout=2)
    assert reaped.is_set()
    assert consumed == (["body"] if stage == "after_feed" else [])
    with pytest.raises(RuntimeError, match="cancelled"):
        runner(["fake-input"], "later")


def test_command_runner_reaps_child_after_unexpected_exception(monkeypatch):
    from thalvi_desktop.host import CommandRunner

    calls = []

    class Child:
        def poll(self):
            return None

        def kill(self):
            calls.append("kill")

        def wait(self):
            calls.append("wait")

        def communicate(self, **_kwargs):
            raise KeyboardInterrupt

    monkeypatch.setattr("thalvi_desktop.host.subprocess.Popen", lambda *_a, **_kw: Child())
    with pytest.raises(KeyboardInterrupt):
        CommandRunner()(["fake-input"], "body")
    assert calls == ["kill", "wait"]
