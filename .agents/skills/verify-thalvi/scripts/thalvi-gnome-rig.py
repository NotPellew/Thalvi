#!/usr/bin/python3 -I
"""Helpers for the GNOME mode of thalvi-desktop-rig; each subcommand runs inside the rig's sandbox.

Daemons (started by the rig): logind, input, mic. Tools: shot, windows, focus, key, type, clip, victim.
"""
import json
import os
import socket
import sys
import time

RUNTIME = "/run/user/%d" % os.getuid()
INPUT_SOCKET = RUNTIME + "/rig-input.sock"
# gnome-shell only answers Screenshot and Introspect calls from the owner of one of these names.
ALLOWED_NAME = "org.freedesktop.impl.portal.desktop.gnome"


def gio():
    import gi

    gi.require_version("Gdk", "3.0")
    from gi.repository import Gdk, Gio, GLib

    return Gdk, Gio, GLib


LOGIND_XML = """<node>
<interface name="org.freedesktop.login1.Manager">
 <method name="GetSession"><arg type="s" direction="in"/><arg type="o" direction="out"/></method>
 <method name="GetSessionByPID"><arg type="u" direction="in"/><arg type="o" direction="out"/></method>
 <method name="GetUser"><arg type="u" direction="in"/><arg type="o" direction="out"/></method>
 <method name="ListSessions"><arg type="a(susso)" direction="out"/></method>
 <method name="Inhibit"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="h" direction="out"/></method>
 <method name="CanSuspend"><arg type="s" direction="out"/></method>
 <method name="CanHibernate"><arg type="s" direction="out"/></method>
 <method name="CanPowerOff"><arg type="s" direction="out"/></method>
 <method name="CanReboot"><arg type="s" direction="out"/></method>
 <signal name="PrepareForSleep"><arg type="b"/></signal>
 <signal name="PrepareForShutdown"><arg type="b"/></signal>
 <property name="IdleHint" type="b" access="read"/>
 <property name="BlockInhibited" type="s" access="read"/>
 <property name="DelayInhibited" type="s" access="read"/>
</interface></node>"""
SESSION_XML = """<node>
<interface name="org.freedesktop.login1.Session">
 <method name="SetIdleHint"><arg type="b" direction="in"/></method>
 <method name="SetLockedHint"><arg type="b" direction="in"/></method>
 <method name="SetType"><arg type="s" direction="in"/></method>
 <method name="Activate"/>
 <method name="TakeControl"><arg type="b" direction="in"/></method>
 <method name="ReleaseControl"/>
 <signal name="Lock"/><signal name="Unlock"/>
 <property name="Id" type="s" access="read"/>
 <property name="Active" type="b" access="read"/>
 <property name="State" type="s" access="read"/>
 <property name="Type" type="s" access="read"/>
 <property name="Class" type="s" access="read"/>
 <property name="Remote" type="b" access="read"/>
 <property name="Name" type="s" access="read"/>
 <property name="Seat" type="(so)" access="read"/>
 <property name="User" type="(uo)" access="read"/>
 <property name="IdleHint" type="b" access="read"/>
 <property name="LockedHint" type="b" access="read"/>
</interface></node>"""
USER_XML = """<node>
<interface name="org.freedesktop.login1.User">
 <property name="UID" type="u" access="read"/><property name="Name" type="s" access="read"/>
 <property name="State" type="s" access="read"/><property name="Sessions" type="a(so)" access="read"/>
 <property name="Display" type="(so)" access="read"/><property name="IdleHint" type="b" access="read"/>
</interface></node>"""


def run_logind():
    """A stand-in for systemd-logind on the sandbox's private system bus: one active graphical session.

    gnome-shell cannot start without logind, and the real one is the developer's session.
    """
    _, Gio, GLib = gio()
    uid = os.getuid()
    session = "/org/freedesktop/login1/session/_31"
    user = "/org/freedesktop/login1/user/_%d" % uid
    fds = []
    props = {
        "/org/freedesktop/login1": {
            "IdleHint": ("b", False), "BlockInhibited": ("s", ""), "DelayInhibited": ("s", ""),
        },
        session: {
            "Id": ("s", "1"), "Active": ("b", True), "State": ("s", "active"), "Type": ("s", "wayland"),
            "Class": ("s", "user"), "Remote": ("b", False), "Name": ("s", "rig"),
            "Seat": ("(so)", ("seat0", "/org/freedesktop/login1/seat/seat0")),
            "User": ("(uo)", (uid, user)), "IdleHint": ("b", False), "LockedHint": ("b", False),
        },
        user: {
            "UID": ("u", uid), "Name": ("s", "rig"), "State": ("s", "active"),
            "Sessions": ("a(so)", [("1", session)]), "Display": ("(so)", ("1", session)), "IdleHint": ("b", False),
        },
    }

    def on_call(conn, sender, path, iface, method, params, inv):
        if method in ("GetSession", "GetSessionByPID"):
            inv.return_value(GLib.Variant("(o)", (session,)))
        elif method == "GetUser":
            inv.return_value(GLib.Variant("(o)", (user,)))
        elif method == "ListSessions":
            inv.return_value(GLib.Variant("(a(susso))", ([],)))
        elif method == "Inhibit":
            read, write = os.pipe()
            fds.append(write)
            inv.return_value_with_unix_fd_list(GLib.Variant("(h)", (0,)), Gio.UnixFDList.new_from_array([read]))
        elif method.startswith("Can"):
            inv.return_value(GLib.Variant("(s)", ("na",)))
        else:
            inv.return_value(None)

    def on_get(conn, sender, path, iface, prop):
        typed = props[path].get(prop)
        return GLib.Variant(*typed) if typed else None

    def on_bus(conn, name):
        for xml, path in ((LOGIND_XML, "/org/freedesktop/login1"), (SESSION_XML, session), (USER_XML, user)):
            for info in Gio.DBusNodeInfo.new_for_xml(xml).interfaces:
                conn.register_object(path, info, on_call, on_get, None)

    Gio.bus_own_name(Gio.BusType.SYSTEM, "org.freedesktop.login1", Gio.BusNameOwnerFlags.NONE, on_bus, None,
                     lambda *_: sys.exit(1))
    GLib.MainLoop().run()


def run_input():
    """Holds a RemoteDesktop session so the seat has a keyboard, and types what arrives on the socket.

    Without it headless mutter has no keyboard: clients never get focus and cannot set the clipboard.
    """
    Gdk, Gio, GLib = gio()
    bus = Gio.bus_get_sync(Gio.BusType.SESSION)
    desktop = "org.gnome.Mutter.RemoteDesktop"

    def call(path, iface, method, args=None, sig=None):
        variant = GLib.Variant(sig, args) if sig else None
        return bus.call_sync(desktop, path, iface, method, variant, None, Gio.DBusCallFlags.NONE, 5000, None)

    path = call("/org/gnome/Mutter/RemoteDesktop", desktop, "CreateSession").unpack()[0]
    iface = desktop + ".Session"
    call(path, iface, "Start")

    def tap(keyval):
        for pressed in (True, False):
            call(path, iface, "NotifyKeyboardKeysym", (keyval, pressed), "(ub)")

    # The first event makes the seat advertise a keyboard.
    tap(Gdk.keyval_from_name("Shift_L"))
    server = socket.socket(socket.AF_UNIX)
    server.bind(INPUT_SOCKET)
    server.listen(4)
    print("ready", flush=True)
    while True:
        conn, _ = server.accept()
        for line in conn.makefile():
            verb, _, arg = line.rstrip("\n").partition(" ")
            if verb == "key":
                tap(Gdk.keyval_from_name(arg))
            elif verb == "type":
                for char in arg:
                    tap(Gdk.unicode_to_keyval(ord(char)))
            conn.sendall(b"ok\n")
        conn.close()


def run_mic(fifo, queue):
    """Plays each .raw file queued in `queue` into the PipeWire microphone's pipe, in real time.

    Samples are 16 kHz mono signed 16-bit. The source reads the pipe only while an app captures, and drops what
    is written faster than it plays: the pipe is kept to one page and the clock restarts after every wait and
    whenever the feeder falls a chunk behind it (after idle time, or a stall), so it never catches up in a burst.
    """
    import fcntl

    rate, chunk_ms = 16000, 20
    chunk_bytes = rate * chunk_ms // 1000 * 2
    fd = None
    pending = b""
    start, tick = time.monotonic(), 0
    while True:
        if not pending:
            names = sorted(n for n in os.listdir(queue) if n.endswith(".raw"))
            if not names:
                time.sleep(chunk_ms / 1000)
                continue
            with open(os.path.join(queue, names[0]), "rb") as f:
                pending = f.read()
            os.unlink(os.path.join(queue, names[0]))
        if time.monotonic() > start + (tick + 1) * chunk_ms / 1000:
            start, tick = time.monotonic(), 0
        try:
            if fd is None:
                fd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)  # fails until an app captures
                fcntl.fcntl(fd, 1031, 4096)  # F_SETPIPE_SZ
            written = os.write(fd, pending[:chunk_bytes])
        except OSError as e:
            if fd is not None and e.errno != 11:  # not EAGAIN: the reader went away
                os.close(fd)
                fd = None
            time.sleep(chunk_ms / 1000)
            start, tick = time.monotonic(), 0
            continue
        pending = pending[written:]
        tick += 1
        time.sleep(max(0.0, start + tick * chunk_ms / 1000 - time.monotonic()))


def shell_call(path, iface, method, args, sig):
    """Calls gnome-shell as the owner of an allowlisted name; returns the unpacked reply."""
    _, Gio, GLib = gio()
    bus = Gio.bus_get_sync(Gio.BusType.SESSION)
    owned = []
    Gio.bus_own_name_on_connection(bus, ALLOWED_NAME, Gio.BusNameOwnerFlags.NONE, lambda *_: owned.append(1), None)
    context = GLib.MainContext.default()
    while not owned:
        context.iteration(True)
    GLib.usleep(400000)  # the shell watches the name asynchronously
    variant = GLib.Variant(sig, args) if sig else None
    return bus.call_sync("org.gnome.Shell", path, iface, method, variant, None, Gio.DBusCallFlags.NONE, 5000,
                         None).unpack()


def windows():
    return shell_call("/org/gnome/Shell/Introspect", "org.gnome.Shell.Introspect", "GetWindows", None, None)[0]


def send_input(line):
    with socket.socket(socket.AF_UNIX) as s:
        s.connect(INPUT_SOCKET)
        s.sendall((line + "\n").encode())
        s.recv(8)


def run_clip():
    """Prints the clipboard text through X11, which reads without taking focus (wl-paste does)."""
    os.environ["GDK_BACKEND"] = "x11"
    import gi

    gi.require_version("Gdk", "3.0")
    gi.require_version("Gtk", "3.0")
    from gi.repository import Gdk, GLib, Gtk

    result = []

    def done(_clipboard, text):
        result.append(text)
        Gtk.main_quit()

    Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD).request_text(done)
    GLib.timeout_add_seconds(3, Gtk.main_quit)
    Gtk.main()
    if not result or result[0] is None:
        sys.exit("clip: the clipboard holds no text")
    sys.stdout.write(result[0])


def run_victim(log_path):
    """A window with a text entry that logs every focus change, for checking what steals focus."""
    import gi

    gi.require_version("Gtk", "3.0")
    from gi.repository import Gtk

    log = open(log_path, "a", buffering=1)

    def stamp(event):
        print("%.3f %s" % (time.time(), event), file=log)

    window = Gtk.Window(title="victim")
    window.set_default_size(600, 300)
    window.add(Gtk.Entry())
    window.connect("focus-in-event", lambda *_: stamp("focus-in"))
    window.connect("focus-out-event", lambda *_: stamp("focus-out"))
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    stamp("mapped")
    Gtk.main()


def main(argv):
    verb = argv[1] if len(argv) > 1 else ""
    if verb == "logind":
        run_logind()
    elif verb == "input":
        run_input()
    elif verb == "mic" and len(argv) == 4:
        run_mic(argv[2], argv[3])
    elif verb == "shot" and len(argv) == 3:
        print(shell_call("/org/gnome/Shell/Screenshot", "org.gnome.Shell.Screenshot", "Screenshot",
                         (False, False, argv[2]), "(bbs)"))
    elif verb == "windows":
        for w in windows().values():
            print("%s\t%s\t%sx%s\t%s" % ("focus" if w.get("has-focus") else "-", w.get("wm-class"), w.get("width"),
                                         w.get("height"), w.get("title")))
    elif verb == "focus":
        print(json.dumps([w.get("title") for w in windows().values() if w.get("has-focus")]))
    elif verb in ("key", "type") and len(argv) == 3:
        send_input("%s %s" % (verb, argv[2]))
    elif verb == "clip":
        run_clip()
    elif verb == "victim" and len(argv) == 3:
        run_victim(argv[2])
    else:
        sys.exit("usage: thalvi-gnome-rig.py logind|input|mic FIFO QUEUE|shot FILE|windows|focus|key NAME|type TEXT|clip|victim LOG")


main(sys.argv)
