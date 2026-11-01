"""Runs the production Discord adapter against a real Slate editor in Chromium.

The fixture is inert: a local CONNECT/TLS proxy serves fake discord.com and
frame.test origins from ``desktop/browser``. The test
uses the production CDP transport and the emitted JavaScript guard, while
Playwright observes the page and drives the fixture's deterministic barriers.
"""

import asyncio
import contextlib
import os
import select
import shutil
import socketserver
import ssl
import subprocess
import threading
import time
from contextlib import asynccontextmanager
from dataclasses import dataclass, replace
from pathlib import Path

import pytest
from playwright.async_api import Page, async_playwright

from thalvi_desktop.config import Config
from thalvi_desktop.discord import (
    DesktopComposer,
    DiscordRefused,
    InsertionUncertain,
    LiveGuarded,
    validate_body,
)
from thalvi_desktop.host import Target
from thalvi_desktop.metrics import Metrics

ROUTE = "https://discord.com/channels/111/222"
OTHER = "https://discord.com/channels/333/444"
BROWSER = Path(__file__).parents[1] / "browser"
DIST = BROWSER / "dist" / "app.js"

pytestmark = pytest.mark.skipif(
    os.environ.get("THALVI_BROWSER_TESTS") != "1",
    reason="set THALVI_BROWSER_TESTS=1 to run real Chromium tests",
)

if os.environ.get("THALVI_BROWSER_TESTS") == "1":
    if not DIST.exists():
        pytest.skip(
            "build the browser fixture with "
            "`npm ci --prefix browser && npm run build --prefix browser`",
            allow_module_level=True,
        )
    if shutil.which("openssl") is None:
        pytest.skip(
            "openssl is required to mint the fake discord.com certificate", allow_module_level=True
        )


class _Proxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, certificate: Path, key: Path):
        self.paths: list[str] = []
        self.blocked: list[str] = []
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.load_cert_chain(str(certificate), str(key))
        self.context.set_alpn_protocols(["http/1.1"])
        super().__init__(("127.0.0.1", 0), _ProxyHandler)


class _ProxyHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        with contextlib.suppress(ssl.SSLError, OSError):
            self._handle()

    def _handle(self) -> None:
        client = self.request
        client.settimeout(5)
        try:
            request = self._read(client)
            if not request:
                return
            line = request.split(b"\r\n", 1)[0].decode("latin1")
            parts = line.split()
            if (
                len(parts) < 2
                or parts[0] != "CONNECT"
                or parts[1] not in {"discord.com:443", "frame.test:443"}
            ):
                self.server.blocked.append(line[:120])
                client.sendall(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                return
            client.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            secure = self.server.context.wrap_socket(client, server_side=True)
            request = self._read(secure)
            if not request:
                return
            head, _, _ = request.partition(b"\r\n\r\n")
            lines = head.split(b"\r\n")
            path = lines[0].decode("latin1").split(" ")[1]
            host = next(
                (
                    value.split(":", 1)[1].strip()
                    for value in (row.decode("latin1") for row in lines[1:])
                    if value.lower().startswith("host:")
                ),
                "",
            )
            if host not in {"discord.com", "frame.test"}:
                self.server.blocked.append(f"host={host}")
                secure.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                return
            self.server.paths.append(path)
            if path == "/app.js":
                body, kind = (BROWSER / "dist" / "app.js").read_bytes(), "application/javascript"
            elif path == "/frame.html":
                body, kind = (BROWSER / "fixture" / "frame.html").read_bytes(), "text/html"
            elif path == "/favicon.ico":
                secure.sendall(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                return
            elif path == "/" or path.startswith("/channels/"):
                body, kind = (BROWSER / "fixture" / "index.html").read_bytes(), "text/html"
            else:
                secure.sendall(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                return
            secure.sendall(
                b"HTTP/1.1 200 OK\r\nContent-Type: "
                + kind.encode()
                + b"; charset=utf-8\r\nContent-Length: "
                + str(len(body)).encode()
                + b"\r\nConnection: close\r\n\r\n"
                + body
            )
        finally:
            with contextlib.suppress(OSError):
                client.close()

    @staticmethod
    def _read(socket) -> bytes:
        data = b""
        while b"\r\n\r\n" not in data and len(data) < 65536:
            chunk = socket.recv(4096)
            if not chunk:
                break
            data += chunk
        return data


def _write_proc(root: Path, port: int, uid: int | None = None) -> Path:
    uid = os.getuid() if uid is None else uid
    net = root / "net"
    net.mkdir(parents=True, exist_ok=True)
    header = "sl local_address rem_address st uid timeout inode\n"
    row = (
        f"0: 0100007F:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000"
        f" 00000000 {uid} 0 4242 1\n"
    )
    (net / "tcp").write_text(header + row)
    (net / "tcp6").write_text(header)
    for pid in (100, 101):
        (root / str(pid) / "fd").mkdir(parents=True)
        os.symlink("/opt/discord/app-1.0.168/Discord", root / str(pid) / "exe")
        (root / str(pid) / "status").write_text(f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n")
    os.symlink("socket:[4242]", root / "101" / "fd" / "61")
    return root


def _certificate(directory: Path) -> tuple[Path, Path]:
    certificate, key = directory / "certificate.pem", directory / "key.pem"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(key),
            "-out",
            str(certificate),
            "-days",
            "1",
            "-subj",
            "/CN=discord.com",
            "-addext",
            "subjectAltName=DNS:discord.com",
        ],
        check=True,
        capture_output=True,
    )
    return certificate, key


def _debug_port(process: subprocess.Popen, timeout: float = 10.0) -> int:
    deadline = time.monotonic() + timeout
    while (remaining := deadline - time.monotonic()) > 0:
        ready, _, _ = select.select([process.stderr], [], [], remaining)
        if not ready:
            continue
        line = process.stderr.readline()
        if not line:
            break
        marker = b"DevTools listening on ws://127.0.0.1:"
        if marker in line:
            return int(line.split(marker, 1)[1].split(b"/", 1)[0])
    raise RuntimeError("Chromium did not publish its debug port.")


def _drain(stream) -> None:
    with contextlib.suppress(OSError, ValueError):
        while stream.read(4096):
            pass


@dataclass
class Live:
    page: Page
    composer: DesktopComposer

    async def fresh(self) -> LiveGuarded:
        await self.page.goto(ROUTE)
        await self.page.wait_for_function("window.__fixture && window.__fixture.ready === true")
        await self.page.evaluate("window.__fixture.focus()")
        return await self.composer._inspect(ROUTE)

    async def state(self) -> dict[str, object]:
        return await self.page.evaluate(
            """({
              model: window.__fixture.model(),
              rendered: window.__fixture.rendered(),
              changes: window.__fixture.changes.slice(),
              events: window.__fixture.events.slice(),
              submits: window.__fixture.submits.slice(),
            })"""
        )

    async def rerender(self) -> None:
        await self.page.evaluate("window.__fixture.rerender()")


@asynccontextmanager
async def live(tmp_path: Path):
    certificate, key = await asyncio.to_thread(_certificate, tmp_path)
    proxy = _Proxy(certificate, key)
    thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    thread.start()
    profile = tmp_path / "profile"
    profile.mkdir()
    playwright = await async_playwright().start()
    process = None
    try:
        executable = playwright.chromium.executable_path
        if not Path(executable).exists():
            pytest.skip("run `uv run playwright install chromium` first")
        process = subprocess.Popen(
            [
                executable,
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--remote-debugging-port=0",
                f"--user-data-dir={profile}",
                "--ignore-certificate-errors",
                f"--proxy-server=http://127.0.0.1:{proxy.server_address[1]}",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--disable-quic",
                "--site-per-process",
                "--metrics-recording-only",
                "--no-startup-window",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        port = await asyncio.to_thread(_debug_port, process)
        threading.Thread(target=_drain, args=(process.stderr,), daemon=True).start()
        browser = await playwright.chromium.connect_over_cdp(f"http://127.0.0.1:{port}")
        context = browser.contexts[0]
        page = await context.new_page()
        await page.goto(ROUTE)
        await page.wait_for_function(
            "window.__fixture && window.__fixture.ready === true", timeout=15000
        )
        config = Config.load(Path(__file__).parents[1] / "config.example.json")
        local = replace(config, discord_port=port)
        composer = DesktopComposer(
            local,
            Metrics(None),
            windows=lambda: [Target("0x1", 100, "discord")],
            root=_write_proc(tmp_path / "proc", port),
            deadline=3.0,
            timeout=3.0,
        )
        try:
            yield Live(page, composer)
        finally:
            await browser.close()
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        await playwright.stop()
        proxy.shutdown()
        proxy.server_close()
        thread.join(timeout=5)


@pytest.mark.parametrize(
    "body",
    ["Hello there!", "Grüße, 世界 👋", "  leading and   inner  ", "keep\u00a0nbsp", "a" * 2000],
)
def test_trusted_cdp_insert_reaches_the_real_slate_model(tmp_path, body):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            try:
                await prepared.session.enter_draft(prepared.guarded, validate_body(body))
                await fixture.rerender()
                state = await fixture.state()
            finally:
                await fixture.composer.close(prepared)
            return state

    state = asyncio.run(behavior())
    assert state["model"] == body
    assert state["rendered"] == body
    assert state["changes"][-1] == body
    assert state["submits"] == []
    assert state["events"] == [
        {
            "trusted": True,
            "inputType": "insertText",
            "data": body,
            "cancelable": True,
            "target": True,
        }
    ]


def test_exec_command_reproduces_the_live_model_loss(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            result = await fixture.page.evaluate(
                "document.execCommand('insertText', false, 'Hello there!')"
            )
            await fixture.rerender()
            state = await fixture.state()
            return result, state

    result, state = asyncio.run(behavior())
    assert result is True
    assert state["model"] == ""
    assert state["changes"] == [""]
    assert state["events"] == []
    assert state["rendered"].startswith("Hello there!")
    assert state["rendered"].endswith("\ufeff")
    assert state["submits"] == []


@pytest.mark.parametrize(
    ("barrier", "reason"),
    [
        ("spa", "route-mismatch"),
        ("focus", "target-mismatch"),
        ("editor", "target-mismatch"),
        ("draft", "draft-mismatch"),
        ("structure", "structure"),
        ("other", "target-mismatch"),
        ("navigation", "document-changed"),
        ("iframe", "uncertain"),
    ],
)
def test_gate_denies_before_slate_mutates(tmp_path, barrier, reason):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            actions = {
                "spa": "history.pushState({}, '', '/channels/333/444')",
                "focus": "document.querySelector('[aria-label=\"More message options\"]').focus()",
                "editor": "window.__fixture.replaceEditor()",
                "draft": "window.__fixture.setDraft('user draft')",
                "structure": (
                    "document.querySelector('form').insertAdjacentHTML("
                    "'beforeend', '<div role=\"dialog\">Confirm</div>')"
                ),
                "other": (
                    "document.body.insertAdjacentHTML('beforeend', "
                    "'<div id=other contenteditable=true></div>');"
                    "document.querySelector('#other').focus()"
                ),
                "iframe": (
                    "new Promise((resolve) => { const frame = document.createElement('iframe');"
                    "frame.src = '/frame.html'; frame.onload = () => resolve(true);"
                    "document.body.appendChild(frame); })"
                ),
            }

            async def change() -> None:
                if barrier == "navigation":
                    await fixture.page.goto(OTHER)
                    await fixture.page.wait_for_function(
                        "window.__fixture && window.__fixture.ready === true"
                    )
                    return
                await fixture.page.evaluate(actions[barrier])
                if barrier == "iframe":
                    await fixture.page.frame_locator("iframe").locator("#frame-editor").focus()

            if barrier == "iframe":
                with pytest.raises(InsertionUncertain):
                    await prepared.session.page.insert(
                        prepared.guarded.editor,
                        prepared.guarded.route,
                        prepared.guarded.digest,
                        "Hello there!",
                        before_input=change,
                    )
                result = None
            else:
                result = await prepared.session.page.insert(
                    prepared.guarded.editor,
                    prepared.guarded.route,
                    prepared.guarded.digest,
                    "Hello there!",
                    before_input=change,
                )
            await fixture.rerender()
            state = await fixture.state()
            frame = await fixture.page.evaluate(
                """({
                  text: document.querySelector('iframe')?.contentDocument
                    ?.querySelector('#frame-editor')?.textContent,
                  gate: document.querySelector('iframe')?.contentWindow
                    ?.__thalvi_discord_gate__?.state,
                })"""
            )
            await fixture.composer.close(prepared)
            return result, state, frame

    result, state, frame = asyncio.run(behavior())
    if barrier == "iframe":
        assert result is None
    else:
        assert result.ok is False
        assert result.reason == reason
        assert result.editor is not None
    if barrier == "draft":
        assert state["model"] == "user draft"
        assert state["rendered"] == "user draft"
    elif barrier == "iframe":
        assert state["model"] == ""
        assert state["rendered"] == ""
        assert frame == {"text": "frame", "gate": "denied"}
    else:
        assert state["model"] == ""
        assert state["rendered"] == ""
    assert "Hello there!" not in state["rendered"]
    if barrier != "navigation":
        assert "Hello there!" not in str(state["changes"])
    assert state["submits"] == []


@pytest.mark.xfail(strict=True, reason="existing child documents are not guarded")
def test_existing_frame_cannot_receive_an_armed_insert(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            await fixture.page.evaluate(
                "new Promise(resolve => { const frame = document.createElement('iframe');"
                "frame.src = '/frame.html'; frame.onload = resolve; "
                "document.body.appendChild(frame); })"
            )
            await fixture.page.evaluate("window.__fixture.focus()")

            async def focus_frame():
                await fixture.page.frame_locator("iframe").locator("#frame-editor").focus()

            with pytest.raises(InsertionUncertain):
                await prepared.session.page.insert(
                    prepared.guarded.editor,
                    prepared.guarded.route,
                    prepared.guarded.digest,
                    "Hello there!",
                    before_input=focus_frame,
                )
            text = (
                await fixture.page.frame_locator("iframe").locator("#frame-editor").text_content()
            )
            await fixture.composer.close(prepared)
            return text

    assert asyncio.run(behavior()) == "frame"


def test_cross_site_frame_is_a_separate_target(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            await fixture.page.evaluate(
                "new Promise(resolve => { const frame = document.createElement('iframe');"
                "frame.src = 'https://frame.test/frame.html'; frame.onload = resolve; "
                "document.body.appendChild(frame); })"
            )
            targets = await prepared.session.page.cdp.call("Target.getTargets")
            await fixture.composer.close(prepared)
            return targets

    targets = asyncio.run(behavior())
    assert any(
        item.get("type") == "iframe" and item.get("url") == "https://frame.test/frame.html"
        for item in targets["targetInfos"]
    )


def test_navigation_denial_leaves_the_new_document_usable(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()

            async def navigate() -> None:
                await fixture.page.goto(OTHER)
                await fixture.page.wait_for_function(
                    "window.__fixture && window.__fixture.ready === true"
                )

            result = await prepared.session.page.insert(
                prepared.guarded.editor,
                prepared.guarded.route,
                prepared.guarded.digest,
                "Hello there!",
                before_input=navigate,
            )
            await fixture.page.keyboard.insert_text("Hello there!")
            await fixture.page.keyboard.press("Enter")
            state = await fixture.state()
            await fixture.composer.close(prepared)
            return result, state

    result, state = asyncio.run(behavior())
    assert result.ok is False
    assert result.reason == "document-changed"
    assert state["model"] == "Hello there!"
    assert state["submits"] == ["Hello there!"]


@pytest.mark.parametrize(
    "body", ["zero\ufeffwidth", "line\nbreak", "line\u2028break", "tab\tseparated"]
)
def test_unsupported_text_is_refused_before_any_browser_mutation(tmp_path, body):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            before = await fixture.state()
            with pytest.raises(ValueError):
                await prepared.session.enter_draft(prepared.guarded, body)
            after = await fixture.state()
            gate = await fixture.page.evaluate("window.__thalvi_discord_gate__ === undefined")
            await fixture.composer.close(prepared)
            return before, after, gate

    before, after, gate = asyncio.run(behavior())
    assert after == before
    assert gate is True


def test_enter_refuses_a_user_edit_after_insertion(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            await prepared.session.enter_draft(prepared.guarded, validate_body("Hello there!"))
            await fixture.page.evaluate("window.__fixture.setDraft(' user edit')")
            with pytest.raises(DiscordRefused):
                await prepared.session.submit(prepared.guarded, "Hello there!")
            state = await fixture.state()
            await fixture.composer.close(prepared)
            return state

    state = asyncio.run(behavior())
    assert state["model"] == "Hello there! user edit"
    assert state["submits"] == []


def test_gate_denial_leaves_typing_and_enter_usable(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()

            async def draft() -> None:
                await fixture.page.evaluate("window.__fixture.setDraft('user draft')")

            result = await prepared.session.page.insert(
                prepared.guarded.editor,
                prepared.guarded.route,
                prepared.guarded.digest,
                "Hello there!",
                before_input=draft,
            )
            await fixture.page.keyboard.type("!")
            await fixture.page.keyboard.press("Enter")
            state = await fixture.state()
            await fixture.composer.close(prepared)
            return result, state

    result, state = asyncio.run(behavior())
    assert result.ok is False
    assert result.reason == "draft-mismatch"
    assert state["model"] == "user draft!"
    assert state["rendered"] == "user draft!"
    assert state["submits"] == ["user draft!"]


def test_pending_gate_does_not_block_unrelated_typing_or_enter(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            page = prepared.session.page
            armed = await page._arm(
                prepared.guarded.editor,
                prepared.guarded.route,
                prepared.guarded.digest,
                "Hello there!",
                "pending",
            )
            await fixture.page.keyboard.type("ordinary")
            await fixture.page.keyboard.press("Enter")
            state = await fixture.state()
            outcome = await page._gate_outcome("pending")
            await page._disarm("pending")
            await fixture.composer.close(prepared)
            return armed, state, outcome

    armed, state, outcome = asyncio.run(behavior())
    assert armed.ok is True
    assert outcome["state"] == "armed"
    assert state["model"] == "ordinary"
    assert state["rendered"] == "ordinary"
    assert state["submits"] == ["ordinary"]


def test_unresolved_input_cannot_be_replaced_by_a_new_gate(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            page = prepared.session.page
            args = (prepared.guarded.editor, prepared.guarded.route, prepared.guarded.digest)
            assert (await page._arm(*args, "old body", "old")).ok
            await page._deny_pending("old")
            second = await page._arm(*args, "new body", "new")
            await fixture.page.evaluate(
                "document.body.insertAdjacentHTML('beforeend', "
                "'<div id=other contenteditable=true></div>'); "
                "document.querySelector('#other').focus()"
            )
            await page.cdp.call("Input.insertText", {"text": "old body"})
            other = await fixture.page.locator("#other").text_content()
            await fixture.page.evaluate("window.__fixture.focus()")
            await fixture.page.keyboard.type("ordinary")
            await fixture.page.keyboard.press("Enter")
            state = await fixture.state()
            await fixture.composer.close(prepared)
            return second, other, state

    second, other, state = asyncio.run(behavior())
    assert second.ok is False
    assert second.reason == "gate-pending"
    assert other == ""
    assert state["model"] == "ordinary"
    assert state["submits"] == ["ordinary"]


def test_cancellation_before_the_wire_leaves_typing_and_enter_usable(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            entered = asyncio.Event()

            async def wait() -> None:
                entered.set()
                await asyncio.Event().wait()

            pending = asyncio.create_task(
                prepared.session.page.insert(
                    prepared.guarded.editor,
                    prepared.guarded.route,
                    prepared.guarded.digest,
                    "Hello there!",
                    before_input=wait,
                )
            )
            await asyncio.wait_for(entered.wait(), 1)
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
            await fixture.page.keyboard.type("ordinary")
            await fixture.page.keyboard.press("Enter")
            state = await fixture.state()
            gate = await fixture.page.evaluate("window.__thalvi_discord_gate__ === undefined")
            await fixture.composer.close(prepared)
            return state, gate

    state, gate = asyncio.run(behavior())
    assert gate is True
    assert state["model"] == "ordinary"
    assert state["rendered"] == "ordinary"
    assert state["submits"] == ["ordinary"]


def test_one_authorization_allows_only_one_trusted_insert(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()

            async def preempt() -> None:
                await prepared.session.page.cdp.call("Input.insertText", {"text": "Hello there!"})

            result = await prepared.session.page.insert(
                prepared.guarded.editor,
                prepared.guarded.route,
                prepared.guarded.digest,
                "Hello there!",
                before_input=preempt,
            )
            await fixture.rerender()
            state = await fixture.state()
            await fixture.composer.close(prepared)
            return result, state

    result, state = asyncio.run(behavior())
    assert result.ok is False
    assert result.reason == "gate-consumed"
    assert state["model"] == "Hello there!"
    assert state["rendered"] == "Hello there!"
    assert state["submits"] == []


def test_enter_is_a_separate_synthetic_press_after_insertion(tmp_path):
    async def behavior():
        async with live(tmp_path) as fixture:
            prepared = await fixture.fresh()
            await prepared.session.enter_draft(prepared.guarded, validate_body("Hello there!"))
            after_insert = await fixture.state()
            notice = await prepared.session.submit(prepared.guarded, "Hello there!")
            after_enter = await fixture.state()
            await fixture.composer.close(prepared)
            return after_insert, notice, after_enter

    after_insert, notice, after_enter = asyncio.run(behavior())
    assert after_insert["submits"] == []
    assert notice is None
    assert after_enter["submits"] == ["Hello there!"]
