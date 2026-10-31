"""Exercises the full adapter against an inert local HTTP, WebSocket, and proc fake."""

import asyncio
import json
import os
from contextlib import asynccontextmanager
from dataclasses import replace
from pathlib import Path

import pytest
from websockets.asyncio.server import serve

from thalvi_desktop.capture import Ending, Request
from thalvi_desktop.config import Config
from thalvi_desktop.discord import (
    AttemptUncertain,
    DesktopComposer,
    DiscordError,
    DiscordRefused,
    DiscordUnavailable,
    InsertionUncertain,
    validate_body,
)
from thalvi_desktop.host import Target
from thalvi_desktop.metrics import Metrics
from thalvi_desktop.workflow import PreviewHost, Workflow

ROUTE = "https://discord.com/channels/1566030981687677039/1566034064333340732"


@pytest.fixture
def config():
    return Config.load(Path(__file__).parents[1] / "config.example.json")


def write_proc(root: Path, port: int, uid: int = 1000) -> Path:
    net = root / "net"
    net.mkdir(parents=True, exist_ok=True)
    header = "sl local_address rem_address st uid timeout inode\n"
    (net / "tcp").write_text(
        header
        + f"0: 0100007F:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000"
        + f" 00000000 {uid} 0 4242 1\n"
    )
    (net / "tcp6").write_text(header)
    for pid in (100, 101):
        (root / str(pid) / "fd").mkdir(parents=True)
        os.symlink("/opt/discord/app-1.0.168/Discord", root / str(pid) / "exe")
        (root / str(pid) / "status").write_text(f"Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n")
    os.symlink("socket:[4242]", root / "101" / "fd" / "61")
    return root


class FakePage:
    def __init__(self, route: str = ROUTE):
        self.route = route
        self.draft = ""
        self.digest = "digest-1"
        self.focused = True
        self.structure = True
        self.entered: list[str] = []
        self.inserted: list[str] = []
        self.inputs: list[str] = []
        self.navigated: list[str] = []
        self.fail_keydown = False
        self.fail_input = False
        self.fail_input_error = False
        self.enter_seen = None
        self.release_enter = None
        self.input_seen = None
        self.release_input = None
        self.gate_state = None
        self.gate_reason = None
        self.gate_nonce = None
        self.armed_text = None
        self.armed_route = None
        self.armed_digest = None
        self.scripts: list[str] = []

    async def handle(self, socket):
        async for raw in socket:
            message = json.loads(raw)
            method, params = message["method"], message.get("params", {})
            if method == "Runtime.evaluate":
                expression = str(params.get("expression", ""))
                if "findEditor()" in expression:
                    remote = (
                        {"type": "object", "objectId": "editor-1"}
                        if self.structure
                        else {"type": "object", "subtype": "null", "value": None}
                    )
                    result = {"result": {"result": remote}}
                elif expression.startswith("({route:"):
                    result = {
                        "result": {
                            "result": {
                                "type": "object",
                                "value": {"route": self.route, "search": "", "hash": ""},
                            }
                        }
                    }
                elif ".gateOutcome(" in expression:
                    result = {
                        "result": {
                            "result": {
                                "type": "object",
                                "value": {
                                    "state": self.gate_state or "missing",
                                    "reason": self.gate_reason,
                                    "events": 1 if self.gate_state else 0,
                                },
                            }
                        }
                    }
                elif ".disarm(" in expression:
                    self.gate_state = None
                    self.gate_reason = None
                    result = {"result": {"result": {"type": "boolean", "value": True}}}
                elif ".denyPending(" in expression:
                    if self.gate_state == "armed":
                        self.gate_state = "denied"
                        self.gate_reason = "cancelled"
                    result = {"result": {"result": {"type": "boolean", "value": True}}}
                else:
                    result = {"error": {"code": -32601, "message": "unknown expression"}}
            elif method == "Runtime.callFunctionOn":
                value = self.observe(params)
                function = str(params["functionDeclaration"])
                if value["ok"] and "guard.arm(" in function:
                    self.armed_text = str(params["arguments"][3]["value"])
                    self.armed_route = str(params["arguments"][0]["value"])
                    self.armed_digest = str(params["arguments"][2]["value"])
                    self.gate_nonce = str(params["arguments"][4]["value"])
                    self.gate_state = "armed"
                    self.gate_reason = None
                elif value["ok"] and "guard.enter(" in function:
                    self.entered.append("keyDown")
                    if self.enter_seen is not None:
                        self.enter_seen.set()
                        await self.release_enter.wait()
                    if self.fail_keydown:
                        await socket.close()
                        return
                result = {"result": {"result": {"type": "object", "value": value}}}
            elif method == "Input.insertText":
                text = str(params["text"])
                self.inputs.append(text)
                if self.fail_input:
                    await socket.close()
                    return
                if self.fail_input_error:
                    await socket.send(
                        json.dumps(
                            {
                                "id": message["id"],
                                "error": {"code": -32602, "message": "invalid input"},
                            }
                        )
                    )
                    continue
                reason = None
                if self.gate_state != "armed":
                    reason = "structure-changed"
                elif text != self.armed_text:
                    reason = "structure"
                elif self.route != self.armed_route:
                    reason = "route-mismatch"
                elif not self.focused:
                    reason = "editor-unfocused"
                elif self.draft:
                    reason = "draft-mismatch"
                elif self.digest != self.armed_digest or not self.structure:
                    reason = "structure-changed"
                if reason is None:
                    self.gate_state = "allowed"
                    self.draft = text
                    self.inserted.append(text)
                else:
                    self.gate_state = "denied"
                    self.gate_reason = reason
                if self.input_seen is not None:
                    self.input_seen.set()
                    await self.release_input.wait()
                result = {"result": {}}
            elif method == "Page.enable":
                result = {"result": {}}
            elif method == "Page.addScriptToEvaluateOnNewDocument":
                identifier = f"script-{len(self.scripts) + 1}"
                self.scripts.append(identifier)
                result = {"result": {"identifier": identifier}}
            elif method == "Page.removeScriptToEvaluateOnNewDocument":
                identifier = str(params.get("identifier"))
                if identifier in self.scripts:
                    self.scripts.remove(identifier)
                result = {"result": {}}
            elif method == "Page.navigate":
                self.route = str(params["url"])
                self.navigated.append(self.route)
                result = {"result": {"frameId": "frame"}}
            else:
                result = {"error": {"code": -32601, "message": "unknown method"}}
            await socket.send(json.dumps({"id": message["id"], **result}))

    def observe(self, params: dict[str, object]) -> dict[str, object]:
        arguments = params["arguments"]
        route = arguments[0]["value"]
        failure = None
        if not self.structure:
            failure = ("structure", False)
        elif self.route != route:
            failure = ("route-mismatch", False)
        elif not self.focused:
            failure = ("editor-unfocused", False)
        if any(
            f"guard.{name}(" in str(params["functionDeclaration"])
            for name in ("verify", "arm", "enter")
        ):
            draft = arguments[1]["value"]
            digest = arguments[2]["value"]
            if failure is None and self.draft != draft:
                failure = ("draft-mismatch", True)
            if failure is None and self.digest != digest:
                failure = ("structure-changed", False)
        if failure is not None:
            return {
                "ok": False,
                "reason": failure[0],
                "retryable": failure[1],
                "draft": "",
                "digest": "",
                "focused": False,
                "route": "",
            }
        return {
            "ok": True,
            "reason": None,
            "retryable": False,
            "draft": self.draft,
            "digest": self.digest,
            "focused": self.focused,
            "route": self.route,
        }


@asynccontextmanager
async def fake_discord(root: Path, page: FakePage):
    documents: dict[str, object] = {}

    def process_request(connection, request):
        if request.path in documents:
            return connection.respond(200, json.dumps(documents[request.path]))
        if request.path == "/devtools/page/abc":
            return None
        return connection.respond(404, "{}")

    async with serve(page.handle, "127.0.0.1", 0, process_request=process_request) as listener:
        port = listener.sockets[0].getsockname()[1]
        documents["/json/version"] = {"Browser": "Chrome/152"}
        documents["/json/list"] = [
            {
                "type": "page",
                "url": page.route,
                "webSocketDebuggerUrl": f"ws://127.0.0.1:{port}/devtools/page/abc",
            }
        ]
        yield port, write_proc(root, port)


def paced(config, port, proc):
    local = replace(config, discord_port=port)
    return local, DesktopComposer(
        local,
        Metrics(None),
        windows=lambda: [Target("0x1", 100, "discord")],
        root=proc,
        deadline=1.0,
        timeout=1.0,
    )


def test_full_adapter_prepares_types_and_closes(config, tmp_path):
    page = FakePage()

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            await prepared.session.enter_draft(prepared.guarded, validate_body("Hello there!"))
            assert page.inserted == ["Hello there!"]
            await composer.close(prepared)

    asyncio.run(behavior())
    assert page.navigated == []


def test_adapter_navigates_to_the_configured_route(config, tmp_path):
    page = FakePage("https://discord.com/channels/1/2")

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            await composer.close(prepared)

    asyncio.run(behavior())
    assert page.navigated == [ROUTE]


def test_adapter_refuses_a_changed_draft_after_the_barrier(config, tmp_path):
    page = FakePage()

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            await prepared.session.enter_draft(prepared.guarded, "Hello there!")
            page.draft = "A user edit"
            with pytest.raises(DiscordRefused, match="changed after typing"):
                await prepared.session.submit(prepared.guarded, "Hello there!")
            await composer.close(prepared)

    asyncio.run(behavior())
    assert page.entered == []


def test_experimental_session_enters_once(config, tmp_path):
    page = FakePage()

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            await prepared.session.enter_draft(prepared.guarded, "Hello there!")
            notice = await prepared.session.submit(prepared.guarded, "Hello there!")
            await composer.close(prepared)
            return notice

    assert asyncio.run(behavior()) is None
    assert page.entered == ["keyDown"]


def test_adapter_reports_an_uncertain_enter_without_a_second_press(config, tmp_path):
    page = FakePage()
    page.fail_keydown = True

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            await prepared.session.enter_draft(prepared.guarded, "Hello there!")
            with pytest.raises(AttemptUncertain):
                await prepared.session.submit(prepared.guarded, "Hello there!")
            await composer.close(prepared)

    asyncio.run(behavior())
    assert page.entered == ["keyDown"]


def test_adapter_reports_an_uncertain_insert_after_connection_loss(config, tmp_path):
    page = FakePage()
    page.fail_input = True

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            with pytest.raises(InsertionUncertain):
                await prepared.session.enter_draft(prepared.guarded, "Hello there!")
            await composer.close(prepared)
            return prepared.session.insert_uncertain

    uncertain = asyncio.run(behavior())
    assert uncertain is True
    assert page.inputs == ["Hello there!"]
    assert page.inserted == []
    assert page.scripts == ["script-1"]


def test_adapter_reports_a_rejected_insert_and_clears_the_gate(config, tmp_path):
    page = FakePage()
    page.fail_input_error = True

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            with pytest.raises(DiscordError, match="rejected the typing command"):
                await prepared.session.enter_draft(prepared.guarded, "Hello there!")
            await composer.close(prepared)

    asyncio.run(behavior())
    assert page.inputs == ["Hello there!"]
    assert page.inserted == []
    assert page.scripts == []
    assert page.gate_state is None


def test_adapter_cancels_after_the_wire_without_a_replay(config, tmp_path):
    page = FakePage()

    async def behavior():
        page.input_seen, page.release_input = asyncio.Event(), asyncio.Event()
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            _, composer = paced(config, port, proc)
            prepared = await composer._inspect(ROUTE)
            pending = asyncio.create_task(
                prepared.session.enter_draft(prepared.guarded, "Hello there!")
            )
            await asyncio.wait_for(page.input_seen.wait(), 1)
            pending.cancel()
            page.release_input.set()
            with pytest.raises(asyncio.CancelledError):
                await pending
            await composer.close(prepared)
            return prepared.session.insert_uncertain, page.gate_state, page.draft

    uncertain, gate, draft = asyncio.run(behavior())
    assert uncertain is True
    assert page.inputs == ["Hello there!"]
    assert page.inserted == ["Hello there!"]
    assert gate == "allowed"
    assert draft == "Hello there!"


def test_adapter_metrics_name_each_call_without_the_body(config, tmp_path):
    page = FakePage()
    path = tmp_path / "metrics.jsonl"

    async def behavior():
        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            local, _ = paced(config, port, proc)
            composer = DesktopComposer(
                local,
                Metrics(path),
                windows=lambda: [Target("0x1", 100, "discord")],
                root=proc,
                deadline=1.0,
                timeout=1.0,
            )
            prepared = await composer._inspect(ROUTE)
            await prepared.session.enter_draft(prepared.guarded, "A private message")
            await prepared.session.submit(prepared.guarded, "A private message")
            await composer.close(prepared)

    asyncio.run(behavior())
    events = [json.loads(line) for line in path.read_text().splitlines()]
    names = {event["event"] for event in events}
    assert {"cdp_http_version", "cdp_http_targets", "cdp_open"} <= names
    assert "cdp_Runtime.callFunctionOn" in names
    assert "discord_window" in names and "discord_listener" in names
    text = path.read_text()
    assert "A private message" not in text
    assert ROUTE not in text


@pytest.mark.parametrize("change", ["route", "draft", "focus"])
def test_guard_refuses_changes_after_the_prepare_observation(config, tmp_path, change):
    page = FakePage()

    async def behavior():
        blocked, release = asyncio.Event(), asyncio.Event()

        class GateMetrics(Metrics):
            async def record(self, event, started, outcome):
                if event == "cdp_Runtime.callFunctionOn" and not blocked.is_set():
                    blocked.set()
                    await release.wait()

        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            local, _ = paced(config, port, proc)
            composer = DesktopComposer(
                local, GateMetrics(None), windows=lambda: [Target("0x1", 100, "discord")], root=proc
            )
            pending = asyncio.create_task(composer._inspect(ROUTE))
            await asyncio.wait_for(blocked.wait(), 1)
            if change == "route":
                page.route = "https://discord.com/channels/1/2"
            elif change == "draft":
                page.draft = "existing"
            else:
                page.focused = False
            release.set()
            prepared = await pending
            with pytest.raises(DiscordRefused):
                await prepared.session.enter_draft(prepared.guarded, "Hello")
            await composer.close(prepared)
            assert page.inserted == []

    asyncio.run(behavior())


def test_live_workflow_refuses_discord_without_connecting_or_confirming(config, monkeypatch):
    async def behavior():
        async def unexpected(*args, **kwargs):
            pytest.fail("Disabled Discord actions must not inspect or connect.")

        def windows():
            pytest.fail("Disabled Discord actions must not inspect desktop windows.")

        composer = DesktopComposer(config, Metrics(None), windows=windows)
        monkeypatch.setattr("thalvi_desktop.discord.cdp.discover", unexpected)
        host = PreviewHost(lambda _: None)
        notices = []
        workflow = Workflow(config, host, notices.append, discord=composer)

        async def confirm():
            pytest.fail("Disabled Discord actions must not request send confirmation.")

        assert not await workflow.finish(
            Request(Ending.TASK, "message the demo channel saying Hello there!"),
            host.capture(),
            confirm,
        )
        assert host.operations == [("clipboard", "Hello there!", False)]
        assert any("Discord typing is disabled" in notice for notice in notices)

        for method in (composer.enter_draft, composer.submit):
            with pytest.raises(DiscordUnavailable, match="disabled"):
                await method(None, "Hello there!")

    asyncio.run(behavior())


@pytest.mark.parametrize("change", ["route", "draft", "focus"])
def test_guard_refuses_changes_after_the_insert_verification(config, tmp_path, change):
    page = FakePage()

    async def behavior():
        blocked, release = asyncio.Event(), asyncio.Event()

        class GateMetrics(Metrics):
            def __init__(self):
                super().__init__(None)
                self.count = 0

            async def record(self, event, started, outcome):
                if event == "cdp_Runtime.callFunctionOn":
                    self.count += 1
                    if self.count == 3:
                        blocked.set()
                        await release.wait()

        async with fake_discord(tmp_path / "proc", page) as (port, proc):
            local, _ = paced(config, port, proc)
            composer = DesktopComposer(
                local, GateMetrics(), windows=lambda: [Target("0x1", 100, "discord")], root=proc
            )
            prepared = await composer._inspect(ROUTE)
            pending = asyncio.create_task(prepared.session.enter_draft(prepared.guarded, "Hello"))
            await asyncio.wait_for(blocked.wait(), 1)
            if change == "route":
                page.route = "https://discord.com/channels/1/2"
            elif change == "draft":
                page.draft = "changed"
            else:
                page.focused = False
            release.set()
            await pending
            with pytest.raises(DiscordRefused):
                await prepared.session.submit(prepared.guarded, "Hello")
            await composer.close(prepared)
            assert page.entered == []

    asyncio.run(behavior())
