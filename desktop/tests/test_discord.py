"""Exercises the bounded Discord composer, process identity, and typed task flow."""

import asyncio
import json
import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from types import SimpleNamespace

import pytest

from thalvi_desktop.capture import Ending, Request
from thalvi_desktop.cdp import CDPTimeout
from thalvi_desktop.config import Config
from thalvi_desktop.discord import (
    AttemptUncertain,
    CDPPage,
    ComposerSession,
    DiscordRefused,
    InsertionUncertain,
    Observation,
    PreviewComposer,
    _listener_inode,
    route_for,
    validate_body,
    verify_process,
)
from thalvi_desktop.host import Target
from thalvi_desktop.metrics import Metrics
from thalvi_desktop.workflow import PreviewHost, Workflow

ROUTE = "https://discord.com/channels/1566030981687677039/1566034064333340732"
DESTINATION = ROUTE
TASK = "message the demo channel saying Hello there!"


def test_composer_guard_does_not_use_exec_command():
    source = Path(__file__).parents[1] / "src" / "thalvi_desktop" / "discord_dom.js"
    assert "execCommand" not in source.read_text(encoding="utf-8")


@pytest.mark.parametrize("method", ["observe", "verify", "arm", "enter"])
def test_emitted_browser_function_executes_for_each_operation(method):
    class Browser:
        async def call(self, name, parameters, **_kwargs):
            assert name == "Runtime.callFunctionOn"
            executed = subprocess.run(
                [
                    "node",
                    "-e",
                    "const p = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));"
                    "const fn = (0,eval)('(' + p.functionDeclaration + ')');"
                    "const result = fn.apply({isConnected:false}, p.arguments.map(a => a.value));"
                    "process.stdout.write(JSON.stringify({result:{value:result}}));",
                ],
                input=json.dumps(parameters),
                text=True,
                capture_output=True,
                check=True,
                timeout=5,
            )
            return json.loads(executed.stdout)

    async def behavior():
        page = CDPPage(Browser(), ROUTE)
        editor = "fixture-editor"
        arguments = {
            "observe": (editor,),
            "verify": (editor, ROUTE, "", "baseline"),
            "arm": (editor, ROUTE, "baseline", "marker", "nonce"),
            "enter": (editor, ROUTE, "baseline", "marker", lambda: None),
        }
        operation = page._arm if method == "arm" else getattr(page, method)
        result = await operation(*arguments[method])
        assert result.ok is False
        assert result.reason == "editor-missing"

    asyncio.run(behavior())


@pytest.fixture
def config():
    return Config.load(Path(__file__).parents[1] / "config.example.json")


def test_route_accepts_channel_and_dm_paths_and_refuses_anything_else():
    assert route_for("https://discord.com/channels/123/456") == "/channels/123/456"
    assert route_for("https://discord.com/channels/@me/456") == "/channels/@me/456"
    for destination in [
        "https://discord.com.evil/channels/1/2",
        "https://discord.com/channels/1/2?x=1",
        "https://discord.com/channels/1/2#x",
        "https://discord.com/channels/1",
        "http://discord.com/channels/1/2",
        "https://discord.com/channels/abc/2",
    ]:
        with pytest.raises(ValueError, match="channel or DM"):
            route_for(destination)


@pytest.mark.parametrize(
    "text,valid",
    [
        ("Hello there!", True),
        ("  spaces and emoji \N{GRINNING FACE} ", True),
        ("", False),
        ("x" * 2000, True),
        ("x" * 2001, False),
        ("line\nbreak", False),
        ("tab\there", False),
        ("bell\x07", False),
        ("delete\x7f", False),
        ("c1\x85", False),
        ("zero\u200bwidth", False),
        ("byte\ufefforder", False),
        ("line\u2028separator", False),
    ],
)
def test_body_validation_refuses_controls_before_any_mutation(text, valid):
    if valid:
        assert validate_body(text) == text
    else:
        with pytest.raises(ValueError):
            validate_body(text)


def write_proc(
    root: Path,
    port: int,
    *,
    window_pid: int = 100,
    owner_pid: int = 100,
    executable: str = "/opt/discord/app-1.0.168/Discord",
    uid: int = 1000,
    address: str = "0100007F",
) -> Path:
    net = root / "net"
    net.mkdir(parents=True, exist_ok=True)
    header = (
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt"
        "   uid  timeout inode\n"
    )
    row = (
        f"   0: {address}:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000"
        f" 00000000 {uid} 0 4242 1 0 0\n"
    )
    (net / "tcp").write_text(header + row)
    (net / "tcp6").write_text(header)
    for pid in {window_pid, owner_pid}:
        (root / str(pid) / "fd").mkdir(parents=True)
        os.symlink(executable, root / str(pid) / "exe")
        (root / str(pid) / "status").write_text(
            f"Name:\tDiscord\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"
        )
    os.symlink("socket:[4242]", root / str(owner_pid) / "fd" / "61")
    return root


def test_process_identity_accepts_the_window_and_its_updater_child(tmp_path):
    root = write_proc(tmp_path / "proc", 9223, window_pid=100, owner_pid=101)
    window = Target("0x1", 100, "discord")
    process = verify_process(9223, [window], ("discord",), root=root, uid=1000)
    assert process.window == window
    assert process.owner == 101
    assert process.executable.name == "Discord"


@pytest.mark.parametrize(
    "windows,executable,address,owner_pid,error",
    [
        ([], "/opt/discord/app-1.0.168/Discord", "0100007F", 100, "No Discord"),
        (
            [Target("0x1", 100, "discord")],
            "/opt/other/app-1.0.168/Other",
            "0100007F",
            100,
            "not the Discord application",
        ),
        (
            [Target("0x1", 100, "discord")],
            "/opt/discord/app-1.0.168/Discord",
            "00000000",
            100,
            "127.0.0.1",
        ),
    ],
)
def test_process_identity_refuses_a_foreign_window_or_binding(
    tmp_path, windows, executable, address, owner_pid, error
):
    root = write_proc(
        tmp_path / "proc",
        9223,
        executable=executable,
        address=address,
        owner_pid=owner_pid,
    )
    with pytest.raises(RuntimeError, match=error):
        verify_process(9223, windows, ("discord",), root=root, uid=1000)


def test_process_identity_refuses_a_listener_owned_by_another_executable(tmp_path):
    root = write_proc(tmp_path / "proc", 9223, window_pid=100, owner_pid=101)
    other = "/opt/other/app-1.0.168/Discord"
    (root / "101" / "exe").unlink()
    os.symlink(other, root / "101" / "exe")
    with pytest.raises(RuntimeError, match="not owned"):
        verify_process(9223, [Target("0x1", 100, "discord")], ("discord",), root=root, uid=1000)


def test_process_identity_refuses_another_uid_and_a_missing_listener(tmp_path):
    root = write_proc(tmp_path / "proc", 9223, uid=2000)
    with pytest.raises(RuntimeError, match="another user"):
        verify_process(9223, [Target("0x1", 100, "discord")], ("discord",), root=root, uid=1000)

    empty = tmp_path / "empty"
    (empty / "net").mkdir(parents=True)
    (empty / "net" / "tcp").write_text("header\n")
    (empty / "net" / "tcp6").write_text("header\n")
    with pytest.raises(RuntimeError, match="not open"):
        _listener_inode(9223, empty)


def observation(page: "ScriptedPage") -> Observation:
    return Observation(
        True, None, False, page.draft, page.digest, page.focused, page.route, page.editor
    )


@dataclass
class ScriptedPage:
    route: str = ROUTE
    editor: str = "editor-1"
    draft: str = ""
    digest: str = "digest-1"
    focused: bool = True
    mismatch: bool = False
    calls: list[object] = field(default_factory=list)
    observe_script: list[Observation] = field(default_factory=list)
    verify_script: list[Observation] = field(default_factory=list)
    gate: asyncio.Event | None = None
    started: asyncio.Event | None = None
    enter_error: Exception | None = None

    async def route_state(self):
        return self.route, "", ""

    async def navigate(self, url):
        self.calls.append(("navigate", url))
        self.route = url

    async def observe(self, editor):
        self.calls.append("observe")
        if self.observe_script:
            return self.observe_script.pop(0)
        return observation(self)

    async def verify(self, editor, route, draft, digest):
        self.calls.append(("verify", draft))
        if self.gate is not None:
            if self.started is not None:
                self.started.set()
            await self.gate.wait()
        if self.verify_script:
            return self.verify_script.pop(0)
        if not self.focused:
            return Observation(
                False, "editor-unfocused", False, self.draft, self.digest, False, self.route, editor
            )
        if self.route != route:
            return Observation(
                False, "route-mismatch", False, self.draft, self.digest, True, self.route, editor
            )
        if self.mismatch or self.draft != draft:
            return Observation(
                False, "draft-mismatch", True, self.draft, self.digest, True, self.route, editor
            )
        if self.digest != digest:
            return Observation(
                False, "structure-changed", False, self.draft, self.digest, True, self.route, editor
            )
        return observation(self)

    async def insert(self, editor, route, digest, text, on_send=None, before_input=None):
        current = await self.verify(editor, route, "", digest)
        if current.ok:
            if on_send is not None:
                on_send()
            self.calls.append(("insert", text))
            self.draft = text
        return current

    async def enter(self, editor, route, digest, text, on_send):
        current = await self.verify(editor, route, text, digest)
        if current.ok:
            on_send()
            self.calls.append("enter")
            if self.enter_error is not None:
                raise self.enter_error
        return current

    async def close(self):
        self.calls.append("close")


def test_session_prepares_an_empty_composer_and_enters_once():
    async def behavior():
        page = ScriptedPage()
        session = ComposerSession(page, ROUTE, deadline=0.5)
        guarded = await session.prepare()
        assert (guarded.route, guarded.editor, guarded.digest) == (
            ROUTE,
            "editor-1",
            "digest-1",
        )
        await session.enter_draft(guarded, "Hello there!")
        await session.submit(guarded, "Hello there!")
        assert page.calls == [
            "observe",
            ("verify", ""),
            ("insert", "Hello there!"),
            ("verify", "Hello there!"),
            ("verify", "Hello there!"),
            "enter",
        ]

    asyncio.run(behavior())


def test_session_refuses_existing_drafts_and_unknown_structure_without_typing():
    async def behavior():
        page = ScriptedPage(draft="A draft I typed")
        with pytest.raises(DiscordRefused, match="already contains a draft"):
            await ComposerSession(page, ROUTE).prepare()
        page = ScriptedPage(
            observe_script=[Observation(False, "structure", False, "", "", False, ROUTE, None)]
        )
        with pytest.raises(DiscordRefused, match="unfamiliar structure"):
            await ComposerSession(page, ROUTE).prepare()
        assert ("insert", "Hello") not in page.calls

    asyncio.run(behavior())


def test_session_retries_an_editor_that_is_still_loading_under_a_deadline():
    async def behavior():
        page = ScriptedPage(
            observe_script=[
                Observation(False, "editor-missing", True, "", "", False, ROUTE, None),
                observation(ScriptedPage()),
            ]
        )
        guarded = await ComposerSession(page, ROUTE, deadline=0.5).prepare()
        assert guarded.editor == "editor-1"
        assert page.calls.count("observe") == 2

    asyncio.run(behavior())


@pytest.mark.parametrize("change", ["route", "draft", "digest", "focus"])
def test_session_refuses_changes_after_the_confirmation_barrier(change):
    async def behavior():
        page = ScriptedPage()
        session = ComposerSession(page, ROUTE, deadline=0.2)
        guarded = await session.prepare()
        await session.enter_draft(guarded, "Hello")
        if change == "route":
            page.route = "https://discord.com/channels/1/2"
        elif change == "draft":
            page.draft = "user edit"
        elif change == "digest":
            page.digest = "different"
        else:
            page.focused = False
        with pytest.raises(DiscordRefused):
            await session.submit(guarded, "Hello")
        assert "enter" not in page.calls

    asyncio.run(behavior())


def test_session_waits_for_the_draft_to_appear_but_never_retries_enter():
    async def behavior():
        page = ScriptedPage()
        insert = page.insert

        async def stale_insert(editor, route, digest, text, on_send=None, before_input=None):
            result = await insert(editor, route, digest, text, on_send=on_send)
            page.draft = ""
            asyncio.get_running_loop().call_later(0.02, setattr, page, "draft", text)
            return result

        page.insert = stale_insert
        session = ComposerSession(page, ROUTE, deadline=0.5)
        guarded = await session.prepare()
        await session.enter_draft(guarded, "Hello")
        assert ("insert", "Hello") in page.calls

        page.mismatch = True
        with pytest.raises(DiscordRefused):
            await session.enter_draft(guarded, "Second")
        assert "enter" not in page.calls

    asyncio.run(behavior())


def test_cancelling_before_enter_sends_no_key():
    async def behavior():
        page = ScriptedPage()
        session = ComposerSession(page, ROUTE, deadline=0.5)
        guarded = await session.prepare()
        await session.enter_draft(guarded, "Hello")
        page.gate, page.started = asyncio.Event(), asyncio.Event()
        task = asyncio.create_task(session.submit(guarded, "Hello"))
        await asyncio.wait_for(page.started.wait(), 1)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert "enter" not in page.calls

    asyncio.run(behavior())


def test_an_uncertain_enter_is_reported_without_a_retry():
    async def behavior():
        page = ScriptedPage(enter_error=CDPTimeout("no answer"))
        session = ComposerSession(page, ROUTE, deadline=0.5)
        guarded = await session.prepare()
        await session.enter_draft(guarded, "Hello")
        with pytest.raises(AttemptUncertain, match="unconfirmed"):
            await session.submit(guarded, "Hello")
        assert page.calls.count("enter") == 1

    asyncio.run(behavior())


class ScriptedComposer:
    def __init__(self, *, prepare_error=None, draft_error=None, submit_error=None):
        self.prepare_error, self.draft_error, self.submit_error = (
            prepare_error,
            draft_error,
            submit_error,
        )
        self.calls: list[tuple[str, object]] = []

    async def prepare(self, destination):
        self.calls.append(("prepare", destination))
        if self.prepare_error is not None:
            raise self.prepare_error
        return object()

    async def enter_draft(self, prepared, body):
        self.calls.append(("draft", body))
        if self.draft_error is not None:
            raise self.draft_error

    async def submit(self, prepared, body):
        self.calls.append(("submit", body))
        if self.submit_error is not None:
            raise self.submit_error
        return "Submission attempted; delivery unconfirmed."

    async def close(self, prepared):
        self.calls.append(("close",))


def run_finish(config, composer, confirm, metrics=None):
    async def behavior():
        host = PreviewHost(lambda _: None)
        notices = []
        workflow = Workflow(config, host, notices.append, discord=composer, metrics=metrics)
        result = await workflow.finish(Request(Ending.TASK, TASK), host.capture(), confirm)
        return result, host, notices

    return asyncio.run(behavior())


def test_workflow_shows_the_destination_and_body_then_reports_an_unconfirmed_attempt(config):
    composer = ScriptedComposer()

    async def confirm():
        return True

    result, host, notices = run_finish(config, composer, confirm)
    assert result is True
    assert host.operations == []
    assert [call[0] for call in composer.calls] == ["prepare", "draft", "submit", "close"]
    assert any(DESTINATION in line and "Hello there!" in line for line in notices)
    assert notices[-1] == "Submission attempted; delivery unconfirmed."


def test_workflow_cancellation_leaves_the_draft_and_presses_nothing(config):
    composer = ScriptedComposer()

    async def confirm():
        return False

    result, host, notices = run_finish(config, composer, confirm)
    assert result is True
    assert host.operations == []
    assert [call[0] for call in composer.calls] == ["prepare", "draft", "close"]
    assert "draft remains in Discord" in notices[-1]


def test_workflow_preparation_and_typing_failures_preserve_the_body(config):
    composer = ScriptedComposer(prepare_error=DiscordRefused("editor-missing"))

    async def confirm():
        raise AssertionError("Preparation failure must not ask for confirmation")

    result, host, notices = run_finish(config, composer, confirm)
    assert result is False
    assert host.operations == [("clipboard", "Hello there!", False)]
    assert "message box was not found" in notices[0]

    composer = ScriptedComposer(draft_error=DiscordRefused("draft-mismatch"))
    result, host, notices = run_finish(config, composer, confirm)
    assert result is False
    assert host.operations == [("clipboard", "Hello there!", False)]
    assert [call[0] for call in composer.calls] == ["prepare", "draft", "close"]
    assert "may remain" in notices[0]


def test_workflow_uncertain_typing_preserves_the_body_and_presses_nothing(config):
    composer = ScriptedComposer(
        draft_error=InsertionUncertain("Typing may have completed; check Discord.")
    )

    async def confirm():
        raise AssertionError("Uncertain typing must not ask for confirmation")

    result, host, notices = run_finish(config, composer, confirm)
    assert result is False
    assert host.operations == [("clipboard", "Hello there!", False)]
    assert any("may have completed" in line for line in notices)
    assert any("Nothing was pressed" in line for line in notices)
    assert [call[0] for call in composer.calls] == ["prepare", "draft", "close"]


@pytest.mark.parametrize(
    ("sent", "uncertain", "notice"),
    [
        (True, True, "Typing may have completed"),
        (True, False, "Typing was interrupted"),
        (False, False, "Cancelled before typing"),
    ],
)
def test_workflow_cancel_during_typing_reports_the_draft_state(config, sent, uncertain, notice):
    class Composer(ScriptedComposer):
        async def prepare(self, destination):
            self.calls.append(("prepare", destination))
            session = SimpleNamespace(insert_sent=sent, insert_uncertain=uncertain)
            return SimpleNamespace(session=session)

        async def enter_draft(self, prepared, body):
            self.calls.append(("draft", body))
            raise asyncio.CancelledError

    composer = Composer()

    async def behavior():
        host = PreviewHost(lambda _: None)
        notices = []
        workflow = Workflow(config, host, notices.append, discord=composer)
        with pytest.raises(asyncio.CancelledError):
            await workflow.finish(Request(Ending.TASK, TASK), host.capture(), confirm)
        return host, notices

    async def confirm():
        return True

    host, notices = asyncio.run(behavior())
    assert host.operations == [("clipboard", "Hello there!", False)]
    assert any(notice in line for line in notices)
    assert calls_include(composer, "close")


def test_workflow_uncertain_and_failed_submits_preserve_the_body_and_close(config):
    composer = ScriptedComposer(
        submit_error=AttemptUncertain("The connection failed while pressing Enter.")
    )

    async def confirm():
        return True

    result, host, notices = run_finish(config, composer, confirm)
    assert result is True
    assert host.operations == [("clipboard", "Hello there!", False)]
    assert any("while pressing Enter" in line for line in notices)
    assert composer.calls[-1][0] == "close"

    composer = ScriptedComposer(submit_error=DiscordRefused("structure-changed"))
    result, host, _ = run_finish(config, composer, confirm)
    assert result is False
    assert host.operations == [("clipboard", "Hello there!", False)]
    assert calls_include(composer, "close")


def calls_include(composer, name):
    return any(call[0] == name for call in composer.calls)


def test_workflow_refuses_invalid_bodies_before_any_composer_call(config):
    composer = ScriptedComposer()

    async def confirm():
        raise AssertionError("An invalid body must not ask for confirmation")

    async def behavior():
        host = PreviewHost(lambda _: None)
        notices = []
        workflow = Workflow(config, host, notices.append, discord=composer)
        result = await workflow.finish(
            Request(Ending.TASK, "message the demo channel saying bad\nbody"),
            host.capture(),
            confirm,
        )
        return result, host, notices

    result, host, notices = asyncio.run(behavior())
    assert result is False
    assert composer.calls == []
    assert host.operations == []
    assert any("control" in line for line in notices)
    assert not any("bad" in line for line in notices)


def test_preview_never_connects_and_reports_an_unconfirmed_attempt(config):
    composer = PreviewComposer()

    async def confirm():
        return True

    result, host, notices = run_finish(config, composer, confirm)
    assert result is True
    assert host.operations == []
    assert composer.operations == [("enter",)]
    assert any("Preview · would insert" in line for line in notices)
    assert not any("Submission attempted" in line for line in notices)


def test_metrics_record_the_discord_outcome_without_content(config, tmp_path):
    path = tmp_path / "metrics.jsonl"
    composer = ScriptedComposer(submit_error=AttemptUncertain("uncertain"))

    async def confirm():
        return True

    run_finish(config, composer, confirm, metrics=Metrics(path))
    events = [json.loads(line) for line in path.read_text().splitlines()]
    assert [(event["event"], event["outcome"]) for event in events] == [
        ("desktop_clipboard", "ok"),
        ("desktop_task", "uncertain"),
    ]
    assert all(isinstance(event["total_ms"], int) for event in events)
    assert "Hello" not in path.read_text()
    assert DESTINATION not in path.read_text()
