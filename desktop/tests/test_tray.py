"""Tests resident tray state and lifecycle without GTK or desktop devices."""

import io
import sys
from types import SimpleNamespace
from unittest.mock import Mock
from xml.etree import ElementTree

import pytest

from thalvi_desktop import gtk_app


@pytest.fixture
def panel(monkeypatch):
    def create(tray_available=True):
        class Application:
            def __init__(self, **kwargs):
                self.send_notification, self.quit = Mock(), Mock()

            def run(self, args):
                return self

        gtk, gio, glib, indicator = Mock(), Mock(), Mock(), Mock()
        gtk.Application = Application
        gtk.MenuItem.side_effect = lambda **kwargs: Mock()
        gio.bus_get_sync.return_value.call_sync.return_value.unpack.return_value = (tray_available,)
        repository = SimpleNamespace(AyatanaAppIndicator3=indicator, Gio=gio, GLib=glib, Gtk=gtk)
        monkeypatch.setitem(sys.modules, "gi", Mock())
        monkeypatch.setitem(sys.modules, "gi.repository", repository)
        monkeypatch.setattr(sys, "argv", ["gtk_app.py", "python", "/config"])
        monkeypatch.setattr(gtk_app.Worker, "start", Mock())
        app = gtk_app.main()
        app.do_activate()
        app.worker.process = Mock(stdin=io.BytesIO())
        app.window.show_all.reset_mock()
        return app, indicator.Indicator.new.return_value, gtk

    return create


def test_public_tray_states_have_distinct_packaged_icons():
    phases = ("paused", "listening", "capturing", "pending", "working", "error")
    states = [gtk_app.tray_state(phase) for phase in phases]
    assert len({icon for icon, _ in states}) == len(phases)
    assert len({description for _, description in states}) == len(phases)
    for phase in gtk_app.PHASES:
        icon, description = gtk_app.tray_state(phase)
        assert ElementTree.parse(icon).getroot().tag.endswith("svg")
        assert description.startswith("Thalvi: ")


def test_tray_updates_countdown_privately_without_presenting_panel(panel, monkeypatch):
    app, indicator, _ = panel()
    clock = [10.0]
    monkeypatch.setattr(gtk_app.time, "monotonic", lambda: clock[0])
    for phase in (
        "loading",
        "paused",
        "listening",
        "capturing",
        "confirming",
        "working",
        "stopped",
        "error",
    ):
        app.worker.receive(
            app.worker.process,
            {"type": "state", "phase": phase, "message": "private speech", "capture_id": 3},
        )
        indicator.set_title.assert_called_with(gtk_app.tray_state(phase)[1])
        indicator.set_icon_full.assert_called_with(*gtk_app.tray_state(phase))
    event = {
        "type": "state",
        "phase": "pending",
        "message": "private speech",
        "capture_id": 3,
        "remaining_seconds": 2.5,
        "target": "Editor",
        "ending": "submit",
    }
    app.worker.receive(app.worker.process, event)
    assert "3 seconds" in indicator.set_title.call_args.args[0]
    clock[0] += 1
    assert app.tick_pending(event)
    assert "2 seconds" in indicator.set_title.call_args.args[0]
    indicator.set_title.reset_mock()
    app.update(app.worker)
    indicator.set_title.assert_not_called()
    app.worker.receive(
        app.worker.process, {"type": "state", "phase": "capturing", "message": "Continued"}
    )
    assert app.tick_pending(event) is False
    indicator.set_title.assert_called_with("Thalvi: Capturing speech")
    app.worker.receive(app.worker.process, {"type": "notice", "message": "private transcript"})
    assert "private" not in indicator.set_icon_full.call_args.args[1]
    app.window.present.assert_not_called()
    app.window.show_all.assert_not_called()


def test_tray_activation_controls_close_and_quit(panel):
    app, indicator, gtk = panel()
    menu = indicator.set_menu.call_args.args[0]
    status, toggle, cancel, quit_item = [call.args[0] for call in menu.append.call_args_list]
    indicator.set_secondary_activate_target.assert_called_once_with(status)
    assert app.close_window() is True
    app.window.hide.assert_called_once()
    status.connect.call_args.args[1]()
    app.window.present.assert_called_once()
    app.worker.phase, app.worker.capture_id = "pending", 4
    app.update(app.worker)
    cancel.set_sensitive.assert_called_with(True)
    cancel.connect.call_args.args[1]()
    toggle.connect.call_args.args[1]()
    assert app.worker.process.stdin.getvalue() == (
        b'{"command": "cancel", "capture_id": 4}\n{"command": "pause"}\n'
    )
    app.worker.quit = Mock()
    quit_item.connect.call_args.args[1]()
    app.worker.quit.assert_called_once()
    app.quit.assert_called_once()
    gtk.ApplicationWindow.assert_called_once()


def test_without_tray_close_keeps_panel_available_and_quit_reaps_worker(panel):
    app, indicator, _ = panel(tray_available=False)
    assert app.close_window() is True
    app.window.hide.assert_not_called()
    app.status.set_text.assert_called_with("System tray unavailable. Use Quit to stop Thalvi.")
    indicator.set_menu.assert_not_called()
    process = app.worker.process
    app.quit_app()
    assert process.stdin.getvalue() == b'{"command": "quit"}\n'
    process.wait.assert_called_once_with(timeout=3)
    assert app.worker.process is None
    app.quit.assert_called_once()
