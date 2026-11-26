#!/usr/bin/env python3
"""Build, serve and exercise signed fixture updates on a private desktop."""

import argparse
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from threading import Thread
import time
from urllib.request import Request, urlopen


WINDOWS = os.name == "nt"
VERSION_A = "91.0.0"
VERSION_B = "91.0.1"
ACCENT = "#ff87de"
ACCENT_A = "#6bf8ff"
ACCENT_B = "#b5ff62"
HEADING_RULE = "#ffdc73"
THEMES = {"version": 1, "holo": {"colors": {"dark": {"accent": ACCENT}}}}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def sha256(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def wait_for(description, read, seconds=60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = read()
        if value:
            return value
        time.sleep(0.25)
    raise RuntimeError(f"Timed out waiting for {description}")


def tauri(app, *args, **kwargs):
    cli = app / "ui/node_modules/@tauri-apps/cli/tauri.js"
    return subprocess.run(["node", cli, *args], cwd=app, **kwargs)


def keys(app):
    result = tauri(app, "signer", "generate", "--ci", capture_output=True, text=True)
    require(result.returncode == 0, "Could not generate a fixture signing key")
    lines = result.stdout.splitlines()
    private = next(index for index, line in enumerate(lines) if line.startswith("Private:"))
    public = next(index for index, line in enumerate(lines) if line.startswith("Public:"))
    return lines[private + 1].strip(), lines[public + 1].strip()


def sign(app, key, package):
    env = os.environ | {"TAURI_SIGNING_PRIVATE_KEY": key, "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": ""}
    env.pop("TAURI_SIGNING_PRIVATE_KEY_PATH", None)
    result = tauri(app, "signer", "sign", "--app-version", VERSION_B, package, env=env, capture_output=True)
    require(result.returncode == 0, f"Could not sign {package.name} with the fixture key")
    return package.with_suffix(package.suffix + ".sig").read_text(encoding="utf-8").strip()


def build(app, work, feed, public_key, version, feature):
    bundle = "nsis" if WINDOWS else "appimage"
    target = Path(os.environ.get("CARGO_TARGET_DIR", app.parent.parent / "target")).resolve()
    env = os.environ | {"CARGO_TARGET_DIR": str(target), "THALVI_UPDATE_FIXTURE_FEED": feed}
    for name in ("TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PATH", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD"):
        env.pop(name, None)
    config = work / "tauri.fixture.json"
    write_json(config, {"version": version, "plugins": {"updater": {
        "pubkey": public_key, "dangerousInsecureTransportProtocol": True,
    }}})
    extra = []
    if WINDOWS:
        extra = ["--config", "tauri.webgpu.windows.json"]
    else:
        env["LD_LIBRARY_PATH"] = str(target / "release")
        env["LINUXDEPLOY_EXCLUDED_LIBRARIES"] = "libvulkan.so*"
    tauri(app, "build", "--ci", "--bundles", bundle, "--features", feature,
          "--config", config, *extra, "--", "--locked", env=env, check=True)
    suffix = "x64-setup.exe" if WINDOWS else "amd64.AppImage"
    name = f"Thalvi_{version}_{suffix}"
    return Path(shutil.copy2(target / "release/bundle" / bundle / name, work / name))


class Feed(BaseHTTPRequestHandler):
    def reply(self, code, length=0, content_type="application/octet-stream"):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(length))
        self.end_headers()

    def do_GET(self):
        if self.path == "/latest.json":
            case = self.server.offer
            offer = self.server.offers.get(case)
            if offer is None:
                self.reply(204)
                return
            platform = "windows-x86_64" if WINDOWS else "linux-x86_64"
            data = json.dumps({"version": VERSION_B, "platforms": {platform: {
                "url": f"{self.server.origin}/package/{case}", "signature": offer[1],
            }}}).encode()
            self.reply(200, len(data), "application/json")
            self.wfile.write(data)
        elif self.path.startswith("/package/") and self.path[9:] in self.server.offers:
            package, _ = self.server.offers[self.path[9:]]
            self.reply(200, package.stat().st_size)
            with package.open("rb") as file:
                shutil.copyfileobj(file, self.wfile)
        else:
            self.reply(404)

    def do_POST(self):
        case = self.path.removeprefix("/offer/")
        if case == "empty" or case in self.server.offers:
            self.server.offer = case
            self.reply(204)
        else:
            self.reply(404)


@contextmanager
def loopback():
    with ThreadingHTTPServer(("127.0.0.1", 0), Feed) as server:
        server.origin = f"http://127.0.0.1:{server.server_port}"
        server.offers = {}
        server.offer = "empty"
        worker = Thread(target=server.serve_forever)
        worker.start()
        try:
            yield server
        finally:
            server.shutdown()
            worker.join()


def run_linux(root, work, package, out, origin):
    rig = root / ".agents/skills/verify-thalvi/scripts/thalvi-desktop-rig"
    env = os.environ | {"THALVI_RIG_BASE": str(work / "rig")}
    output = subprocess.check_output([rig, "up", "--gnome", "--software", "--size", "1280x800"], env=env, text=True)
    run = Path(next(line.removeprefix("RUN_DIR=") for line in output.splitlines() if line.startswith("RUN_DIR=")))
    try:
        shutil.copy2(package, run / "home/Thalvi.AppImage")
        shutil.copy2(__file__, run / "home/update-fixture.py")
        subprocess.run([rig, "sh", run, "/usr/bin/python3", "/home/rig/update-fixture.py", "drive",
                        "/home/rig/Thalvi.AppImage", "/tmp/out", origin], env=env, check=True)
    finally:
        try:
            for path in (run / "out").iterdir():
                if path.is_file() and path.suffix in {".json", ".jsonl", ".log", ".png"}:
                    shutil.copy2(path, out / path.name)
        finally:
            subprocess.run([rig, "down", run], env=env, check=True)


def suite(out):
    root = Path(__file__).resolve().parents[3]
    app = root / "crates/thalvi-app"
    out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="thalvi-update-", dir=None if WINDOWS else "/var/tmp") as scratch, loopback() as server:
        work = Path(scratch)
        secret, public = keys(app)
        other, _ = keys(app)
        feed = f"{server.origin}/latest.json"
        a = build(app, work, feed, public, VERSION_A, "update-fixture")
        b = build(app, work, feed, public, VERSION_B, "update-fixture-b")
        invalid = work / "invalid-package"
        invalid.write_bytes(b"This signed update payload is not an installer.\n")
        server.offers = {
            "signature": (b, sign(app, other, b)),
            "install": (invalid, sign(app, secret, invalid)),
            "success": (b, sign(app, secret, b)),
        }
        del secret, other
        packages = {
            "a": {"file": a.name, "version": VERSION_A, "sha256": sha256(a)},
            "b": {"file": b.name, "version": VERSION_B, "sha256": sha256(b)},
            "invalid_sha256": sha256(invalid),
        }
        write_json(out / "packages.json", packages)
        if WINDOWS:
            subprocess.run(["pwsh", "-NoProfile", "-File", root / ".github/scripts/windows-smoke.ps1",
                            "-Installer", a, "-OutDir", out, "-UpdateFeed", server.origin,
                            "-ExpectedInstaller", f"Thalvi-{VERSION_B}-installer.exe"], check=True)
        else:
            run_linux(root, work, a, out, server.origin)
            receipt = json.loads((out / "update-receipt.json").read_text(encoding="utf-8"))
            require(receipt["success"]["installed_sha256"] == packages["b"]["sha256"],
                    "The updated AppImage does not match package B")


class Desktop:
    def __init__(self, package, out, origin):
        self.out = out
        self.origin = origin
        self.env = os.environ.copy()
        self.app = None
        if WINDOWS:
            self.exe = Path(os.environ["LOCALAPPDATA"]) / "Programs/Thalvi/thalvi-desktop.exe"
            self.config = Path(os.environ["APPDATA"]) / "thalvi/desktop.json"
            self.metrics = Path(os.environ["LOCALAPPDATA"]) / "thalvi/recordings/metrics.jsonl"
            require(not self.exe.exists() and not self.config.exists(), "The fixture needs a fresh Windows test account")
            installer = subprocess.run([package, "/S"], timeout=120)
            write_json(out / "initial-installer.json", {"version": VERSION_A, "exit_code": installer.returncode})
            installer.check_returncode()
            self.launch = self.exe
        else:
            self.launch = package
            self.config = Path.home() / ".config/thalvi/desktop.json"
            self.metrics = Path.home() / ".local/share/thalvi/recordings/metrics.jsonl"
            self.env.update(APPIMAGE_EXTRACT_AND_RUN="1", TMPDIR=str(Path.home() / "tmp"))
            Path(self.env["TMPDIR"]).mkdir()
            extracted = Path.home() / "control"
            extracted.mkdir()
            subprocess.run([package, "--appimage-extract"], cwd=extracted, stdout=subprocess.DEVNULL, check=True)
            self.exe = extracted / "squashfs-root/AppRun"
        self.config.parent.mkdir(parents=True, exist_ok=True)
        write_json(self.config, {"wake": {"enabled": False}, "recordings": {"metrics": True}, "stt": {"entries": [
            {"kind": "realtime", "id": "fixture", "name": "Update fixture", "active": True,
             "address": "ws://127.0.0.1:9", "model": "unused"},
        ]}})

    def start(self, case):
        with urlopen(Request(f"{self.origin}/offer/{case}", method="POST"), timeout=5) as reply:
            require(reply.status == 204, f"Could not publish {case} fixture")
        with (self.out / f"{case}.log").open("wb") as log:
            self.app = subprocess.Popen([self.launch], stdout=log, stderr=subprocess.STDOUT, env=self.env)
        wait_for(f"{case} app to answer settings get", lambda: self.cli("settings", "get"))

    def cli(self, *args, change=None):
        response = self.reply(*args, change=change)
        return response if response and response.get("ok") else None

    def reply(self, *args, change):
        try:
            reply = subprocess.run([self.exe, *args], input=None if change is None else json.dumps(change),
                                   capture_output=True, encoding="utf-8", timeout=15, env=self.env)
        except (PermissionError, subprocess.TimeoutExpired):
            return None
        for line in reversed(reply.stdout.splitlines()):
            if line.startswith("{"):
                return json.loads(line)
        return None

    def save(self, change):
        require(self.cli("settings", "set", "-", change=change), f"Could not save {change}")

    def stop(self):
        if self.cli("quit"):
            wait_for("the current Thalvi to stop", lambda: self.cli("settings", "get") is None)
        if self.app is not None:
            app, self.app = self.app, None
            try:
                code = app.wait(timeout=10)
                require(code == 0, f"The original Thalvi process exited with {code}")
                return code
            finally:
                if app.poll() is None:
                    app.kill()
                    app.wait()

    def metric(self, step, outcome, current):
        try:
            lines = self.metrics.read_text(encoding="utf-8").splitlines()
        except FileNotFoundError:
            return None
        for line in lines:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (event.get("event"), event.get("step"), event.get("outcome"), event.get("current"), event.get("version")) == (
                "desktop_update", step, outcome, current, VERSION_B,
            ):
                return event
        return None

    def screenshot(self, name, colors):
        from PIL import Image, ImageGrab

        require(self.cli("show", "app"), "Could not open the App page in Settings")
        path = self.out / f"{name}.png"
        expected = [tuple(bytes.fromhex(color[1:])) for color in colors]

        def rendered():
            if WINDOWS:
                ImageGrab.grab().save(path)
            else:
                subprocess.run(["/tmp/thalvi-gnome-rig.py", "shot", path], stdout=subprocess.DEVNULL, check=True)
            with Image.open(path) as shot:
                counts = dict((color, count) for count, color in shot.convert("RGB").getcolors(shot.width * shot.height))
            return all(counts.get(color, 0) >= 12 for color in expected)

        wait_for(f"{name} Settings pixels {', '.join(colors)}", rendered, 30)

    def stored(self):
        return json.loads(self.config.read_text(encoding="utf-8"))

    def theme_intact(self):
        require(self.stored()["themes"] == THEMES, "The saved sparse theme changed")
        live = self.cli("settings", "get")
        require(live and live["settings"]["themes"] == THEMES, "The running app lost the theme")


def drive(package, out, origin):
    desktop = Desktop(package, out, origin)
    receipt = {}
    new_property = {"themes": {"version": 1, "holo": {"colors": {"dark": {"heading_rule": HEADING_RULE}}}}}
    try:
        desktop.start("empty")
        desktop.screenshot("before-default", [ACCENT_A])
        desktop.save({"look": "holo", "themes": THEMES})
        desktop.theme_intact()
        checked = desktop.reply("settings", "check", "-", change=new_property)
        require(checked and checked.get("valid") is False and "heading_rule" in checked["error"],
                "Package A did not reject the property introduced in B")
        desktop.screenshot("before-custom", [ACCENT])
        receipt["prepare_exit_code"] = desktop.stop()
        baseline = sha256(desktop.config)
        for case, step, outcome in [("signature", "download", "signature"), ("install", "install", "not_applied")]:
            desktop.start(case)
            event = wait_for(case, lambda: desktop.metric(step, outcome, VERSION_A), 120)
            wait_for("A to answer after the failure", lambda: desktop.cli("settings", "get"))
            require(case != "signature" or desktop.app.poll() is None, "A exited after rejecting the signature")
            if case == "install":
                require(desktop.metric("download", "ok", VERSION_A), "The invalid installer did not pass verification")
            desktop.theme_intact()
            after = sha256(desktop.config)
            require(after == baseline, f"{case} changed desktop.json")
            receipt[case] = {"update": event, "before_sha256": baseline, "after_sha256": after,
                             "app_exit_code": desktop.stop()}
            write_json(out / "update-receipt.json", receipt)
        desktop.start("success")
        event = wait_for("B to report a successful install after restart", lambda: desktop.metric("install", "ok", VERSION_B), 180)
        wait_for("B to answer settings get", lambda: desktop.cli("settings", "get"))
        desktop.theme_intact()
        require(desktop.cli("settings", "check", "-", change=new_property),
                "Package B did not accept the new theme property")
        desktop.screenshot("after-custom", [ACCENT, HEADING_RULE])
        desktop.save({"look": "gunmetal"})
        desktop.screenshot("after-default", [ACCENT_B])
        desktop.save({"look": "holo"})
        desktop.save({"overlay": {"draft": False}})
        require(desktop.stored()["overlay"]["draft"] is False, "The unrelated settings save did not land")
        desktop.theme_intact()
        desktop.screenshot("after-unrelated-save", [ACCENT, HEADING_RULE])
        receipt["success"] = {"update": event, "app_exit_code": desktop.stop(), "theme": THEMES}
        if not WINDOWS:
            receipt["success"]["installed_sha256"] = sha256(package)
        write_json(out / "update-receipt.json", receipt)
    finally:
        if desktop.app is not None:
            desktop.stop()
        if desktop.metrics.exists():
            shutil.copy2(desktop.metrics, out / "update-metrics.jsonl")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("suite").add_argument("out", type=Path)
    installed = commands.add_parser("drive")
    installed.add_argument("package", type=Path)
    installed.add_argument("out", type=Path)
    installed.add_argument("origin")
    args = parser.parse_args()
    if args.command == "suite":
        suite(args.out.resolve())
    else:
        drive(args.package, args.out, args.origin)


if __name__ == "__main__":
    main()
