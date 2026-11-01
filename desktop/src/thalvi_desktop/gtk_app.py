"""Owns the resident GTK panel, tray, and worker process."""

import contextlib
import json
import math
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

PHASES = {
    "loading",
    "paused",
    "listening",
    "capturing",
    "pending",
    "confirming",
    "working",
    "error",
    "stopped",
}
ACTIVE = {"listening", "capturing", "pending", "confirming", "working"}
CAPTURE = {"capturing", "pending", "confirming", "working"}
TRAY_STATES = {
    "loading": ("working", "Loading"),
    "paused": ("paused", "Paused"),
    "listening": ("listening", "Listening for wake words"),
    "capturing": ("capturing", "Capturing speech"),
    "pending": ("pending", "Ending pending"),
    "confirming": ("pending", "Waiting for confirmation"),
    "working": ("working", "Working"),
    "error": ("error", "Needs attention"),
    "stopped": ("paused", "Stopped"),
}


def tray_state(phase, remaining=0):
    icon, description = TRAY_STATES[phase]
    if phase == "pending":
        description += f" — {math.ceil(max(0, remaining))} seconds; keep speaking or Cancel to stop"
    return str(Path(__file__).with_name("icons") / f"thalvi-{icon}.svg"), f"Thalvi: {description}"


def pending_prompt(event, seconds):
    target = event.get("target", "the app")
    ending = event.get("ending", "action")
    action = "send" if ending == "submit" else "continue"
    return (
        f"Confirm {ending} to {target}: pause {seconds:.1f} seconds to {action}. "
        "Keep speaking or Cancel to continue."
    )


class Worker:
    def __init__(self, python, config, dispatch, changed):
        self.python, self.config = python, config
        self.dispatch, self.changed = dispatch, changed
        self.process = None
        self.phase, self.message = "loading", "Loading the wake model."
        self.preview, self.capture_id, self.event = "", None, {}
        self.quitting = False

    def start(self):
        try:
            process = subprocess.Popen(
                [self.python, "-m", "thalvi_desktop.app_worker", "--config", self.config],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
        except OSError as error:
            self.phase, self.message = "error", f"Could not start the desktop worker: {error}"
            self.changed(self)
            return
        self.process = process
        threading.Thread(target=self.read, args=(process,), daemon=True).start()

    def read(self, process):
        invalid = False
        while line := process.stdout.readline(100_001):
            try:
                if len(line) > 100_000 or not line.endswith(b"\n"):
                    raise ValueError
                event = json.loads(line)
                if (
                    not isinstance(event, dict)
                    or event.get("type") not in ("state", "notice")
                    or not isinstance(event.get("message"), str)
                ):
                    raise ValueError
                if event["type"] == "state" and event.get("phase") not in PHASES:
                    raise ValueError
                if "capture_id" in event and (
                    type(event["capture_id"]) is not int or event["capture_id"] < 1
                ):
                    raise ValueError
                if "remaining_seconds" in event and (
                    type(event["remaining_seconds"]) not in (int, float)
                    or not math.isfinite(event["remaining_seconds"])
                ):
                    raise ValueError
                if any(k in event and not isinstance(event[k], str) for k in ("target", "ending")):
                    raise ValueError
            except (ValueError, UnicodeDecodeError):
                invalid = True
                break
            self.dispatch(self.receive, process, event)
        if invalid:
            self.stop_process(process)
        process.wait()
        self.dispatch(self.exited, process, invalid)

    def receive(self, process, event):
        if process is not self.process or self.quitting:
            return False
        if event["type"] == "notice":
            self.preview = event["message"]
        else:
            if event["phase"] == "stopped" and self.phase == "error":
                return False
            self.phase, self.message = event["phase"], event["message"]
            self.capture_id, self.event = event.get("capture_id"), event
            if self.phase == "capturing":
                self.preview = ""
        self.changed(self)
        return False

    def exited(self, process, invalid):
        if process is not self.process or self.quitting:
            return False
        self.process = None
        was_error = self.phase == "error"
        self.phase = "error"
        if was_error and not invalid:
            self.changed(self)
            return False
        self.message = (
            "The desktop worker sent an invalid response."
            if invalid
            else "The desktop worker stopped. Restart Thalvi to try again."
        )
        self.changed(self)
        return False

    def command(self, command):
        if not self.process or self.quitting:
            return
        if command == "cancel" and (self.phase not in CAPTURE or self.capture_id is None):
            return
        event = {"command": command}
        if command == "cancel":
            event["capture_id"] = self.capture_id
        try:
            self.process.stdin.write((json.dumps(event) + "\n").encode())
            self.process.stdin.flush()
        except (BrokenPipeError, OSError):
            self.phase, self.message = (
                "error",
                "The desktop worker stopped. Restart Thalvi to try again.",
            )
            self.changed(self)

    @staticmethod
    def stop_process(process):
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGINT)
                process.wait(timeout=3)
            except (OSError, subprocess.TimeoutExpired):
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait()

    def quit(self):
        if self.quitting:
            return
        self.command("quit")
        self.quitting = True
        process, self.process = self.process, None
        if process:
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.stop_process(process)


def main():
    if len(sys.argv) != 3:
        print("Usage: gtk_app.py BACKEND_PYTHON CONFIG", file=sys.stderr)
        return 2
    try:
        import gi

        gi.require_version("Gtk", "3.0")
        gi.require_version("Gdk", "3.0")
        gi.require_version("AyatanaAppIndicator3", "0.1")
        from gi.repository import AyatanaAppIndicator3 as Indicator
        from gi.repository import Gio, GLib, Gtk
    except (ImportError, ValueError) as error:
        print(f"Thalvi desktop requires GTK 3 and Ayatana AppIndicator: {error}", file=sys.stderr)
        return 1

    class App(Gtk.Application):
        def __init__(self):
            super().__init__(
                application_id="org.thalvi.Desktop", flags=Gio.ApplicationFlags.FLAGS_NONE
            )
            self.window, self.worker = None, None
            self.tray_available, self.last_phase = False, None
            self.last_tray_state = None
            self.pending_event, self.pending_deadline = None, 0.0

        def do_activate(self):
            if self.window:
                self.window.present()
                return
            self.window = Gtk.ApplicationWindow(application=self, title="Thalvi")
            self.window.set_default_size(470, 390)
            self.window.set_border_width(18)
            self.window.connect("delete-event", self.close_window)
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
            self.window.add(box)
            title = Gtk.Label(label="Thalvi")
            title.set_xalign(0)
            title.get_style_context().add_class("title")
            box.pack_start(title, False, False, 0)
            self.status = Gtk.Label(label="Loading the wake model.")
            self.status.set_xalign(0)
            self.status.set_line_wrap(True)
            box.pack_start(self.status, False, False, 0)
            self.detail = Gtk.Label()
            self.detail.set_xalign(0)
            self.detail.set_line_wrap(True)
            box.pack_start(self.detail, False, False, 0)
            controls = Gtk.Box(spacing=8)
            self.toggle = Gtk.Button(label="Enable listening")
            self.toggle.connect("clicked", self.toggle_listening)
            controls.pack_start(self.toggle, False, False, 0)
            self.cancel = Gtk.Button(label="Cancel capture")
            self.cancel.connect("clicked", lambda *_: self.worker.command("cancel"))
            controls.pack_start(self.cancel, False, False, 0)
            quit_button = Gtk.Button(label="Quit")
            quit_button.connect("clicked", lambda *_: self.quit_app())
            controls.pack_start(quit_button, False, False, 0)
            box.pack_start(controls, False, False, 0)
            instruction = Gtk.Label(
                label="Click in an input, say Hey Thalvi, then finish with paste that, "
                "submit, or do that and pause 2.5 seconds."
            )
            instruction.set_xalign(0)
            instruction.set_line_wrap(True)
            box.pack_start(instruction, False, False, 0)
            settings = Gtk.Label()
            settings.set_xalign(0)
            settings.set_line_wrap(True)
            settings.set_selectable(True)
            settings.set_text(self.config_summary())
            expander = Gtk.Expander(label="Connection and configuration")
            expander.add(settings)
            box.pack_start(expander, False, False, 0)
            self.preview = Gtk.Label()
            self.preview.set_xalign(0)
            self.preview.set_line_wrap(True)
            self.preview.set_selectable(True)
            box.pack_start(self.preview, True, True, 0)
            self.setup_tray()
            self.worker = Worker(sys.argv[1], sys.argv[2], GLib.idle_add, self.update)
            self.update(self.worker)
            self.worker.start()
            self.window.show_all()

        def config_summary(self):
            try:
                raw = json.loads(Path(sys.argv[2]).read_text())
                wake = raw.get("wake", {})
                endings = raw.get(
                    "endings", {"paste": "paste", "submit": "submit", "do that": "task"}
                )
                return (
                    f"Config: {sys.argv[2]}\nWake model: {wake.get('capture_model', 'default')}\n"
                    f"Microphone: {raw.get('device', 'default')}\nServer: {raw.get('server', 'ws://127.0.0.1:8080')}\n"
                    f"Endings: {', '.join(endings)}"
                )
            except (OSError, ValueError, AttributeError, TypeError):
                return f"Config: {sys.argv[2]}"

        def setup_tray(self):
            try:
                bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
                result = bus.call_sync(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "NameHasOwner",
                    GLib.Variant("(s)", ("org.kde.StatusNotifierWatcher",)),
                    GLib.VariantType.new("(b)"),
                    Gio.DBusCallFlags.NONE,
                    1000,
                    None,
                )
                self.tray_available = result.unpack()[0]
                if not self.tray_available:
                    return
                self.indicator = Indicator.Indicator.new(
                    "thalvi",
                    tray_state("loading")[0],
                    Indicator.IndicatorCategory.APPLICATION_STATUS,
                )
                self.indicator.set_status(Indicator.IndicatorStatus.ACTIVE)
                menu = Gtk.Menu()
                status_item = Gtk.MenuItem(label="Open status / settings")
                status_item.connect("activate", lambda *_: self.window.present())
                menu.append(status_item)
                self.tray_toggle = Gtk.MenuItem(label="Enable listening")
                self.tray_toggle.connect("activate", self.toggle_listening)
                menu.append(self.tray_toggle)
                self.tray_cancel = Gtk.MenuItem(label="Cancel capture")
                self.tray_cancel.connect("activate", lambda *_: self.worker.command("cancel"))
                menu.append(self.tray_cancel)
                item = Gtk.MenuItem(label="Quit")
                item.connect("activate", lambda *_: self.quit_app())
                menu.append(item)
                menu.show_all()
                self.indicator.set_menu(menu)
                self.indicator.set_secondary_activate_target(status_item)
            except Exception:
                self.tray_available = False

        def close_window(self, *_):
            if self.tray_available:
                self.window.hide()
            else:
                self.status.set_text("System tray unavailable. Use Quit to stop Thalvi.")
            return True

        def toggle_listening(self, *_):
            phase = self.worker.phase
            if phase == "paused":
                self.worker.command("listen")
                self.toggle.set_sensitive(False)
                if self.tray_available:
                    self.tray_toggle.set_sensitive(False)
            elif phase in ACTIVE:
                self.worker.command("pause")

        def update(self, worker):
            phase = worker.phase
            active = phase in ACTIVE
            self.status.set_text(f"{phase.capitalize()} — {worker.message}")
            event = worker.event
            detail, remaining = "", 0
            if phase == "pending":
                if event is not self.pending_event:
                    self.pending_event = event
                    self.pending_deadline = time.monotonic() + event.get("remaining_seconds", 2.5)
                    GLib.timeout_add(100, self.tick_pending, event)
                remaining = max(0.0, self.pending_deadline - time.monotonic())
                detail = pending_prompt(event, remaining)
            self.detail.set_text(detail)
            self.preview.set_text(worker.preview)
            self.toggle.set_label("Pause listening" if active else "Enable listening")
            self.toggle.set_sensitive(phase == "paused" or active)
            self.cancel.set_sensitive(phase in CAPTURE and worker.capture_id is not None)
            if self.tray_available:
                state = tray_state(phase, remaining)
                if state != self.last_tray_state:
                    icon, description = state
                    self.indicator.set_icon_full(icon, description)
                    self.indicator.set_title(description)
                    self.last_tray_state = state
                self.tray_toggle.set_label("Pause listening" if active else "Enable listening")
                self.tray_toggle.set_sensitive(phase == "paused" or active)
                self.tray_cancel.set_sensitive(phase in CAPTURE and worker.capture_id is not None)
            if phase != self.last_phase and phase in CAPTURE | {"working", "error"}:
                notification = Gio.Notification.new("Thalvi: " + phase.capitalize())
                notification.set_body(
                    pending_prompt(event, event.get("remaining_seconds", 2.5))
                    if phase == "pending"
                    else "Open Thalvi for status."
                )
                self.send_notification("state", notification)
            self.last_phase = phase

        def tick_pending(self, event):
            if self.worker.phase != "pending" or self.worker.event is not event:
                return False
            self.update(self.worker)
            return time.monotonic() < self.pending_deadline

        def quit_app(self):
            if self.worker:
                self.worker.quit()
            self.quit()

    return App().run([sys.argv[0]])


if __name__ == "__main__":
    sys.exit(main())
