"""Exercise the resident command protocol with fake wake and audio flows."""

import asyncio
import json
from types import SimpleNamespace

import pytest

from thalvi_desktop import app_worker as worker


@pytest.mark.parametrize(
    "line",
    [
        b"{}\n",
        b'{"command":"bad"}\n',
        b'{"command":"cancel","capture_id":true}\n',
        b'{"command":"listen","capture_id":1}\n',
        b"x" * 4097,
    ],
)
def test_rejects_invalid_commands(line):
    with pytest.raises(ValueError):
        worker.parse_command(line)


def test_bounded_queue_fails_closed():
    async def run():
        commands = worker.CommandInput(1)
        commands.put(worker.Command(worker.CommandKind.LISTEN))
        commands.put(worker.Command(worker.CommandKind.PAUSE))
        assert await commands.get() is None
        commands.put(worker.Command(worker.CommandKind.LISTEN))
        assert commands.queue.empty()

    asyncio.run(run())


@pytest.mark.parametrize("failure", [False, True])
def test_preload_records_its_real_duration_once(monkeypatch, capsys, failure):
    clock = [10.0]
    records = []

    def load(_config):
        clock[0] = 12.0
        if failure:
            raise RuntimeError("model unavailable")
        return object()

    class Metrics:
        def __init__(self, _path):
            pass

        async def record(self, event, started, outcome):
            records.append((event, clock[0] - started, outcome))

    monkeypatch.setattr(worker, "monotonic", lambda: clock[0])
    monkeypatch.setattr(worker, "Metrics", Metrics)
    monkeypatch.setattr(worker, "Wake", load)
    monkeypatch.setattr(worker, "desktop", lambda: object())

    async def run():
        commands = worker.CommandInput()
        commands.put(worker.Command(worker.CommandKind.QUIT))
        await worker.serve(SimpleNamespace(metrics=None), commands)

    asyncio.run(run())
    assert records == [("desktop_wake_load", 2.0, "error" if failure else "ok")]


def test_resident_cancel_pause_resume_and_quit(monkeypatch, capsys):
    async def run():
        starts = asyncio.Queue()
        stopped = asyncio.Queue()
        resets = []

        class Wake:
            def __init__(self, config):
                pass

            def reset(self):
                resets.append(True)

        class Runner:
            def cancel(self):
                pass

        monkeypatch.setattr(worker, "Wake", Wake)
        monkeypatch.setattr(worker, "desktop", lambda: object())
        monkeypatch.setattr(worker, "CommandRunner", Runner)
        monkeypatch.setattr(worker, "Hyprland", lambda runner: object())
        original_notice = worker.cli.notice

        async def fake_run(config, dry, once, resident):
            resident.begin()
            await starts.put(resident.capture_id)
            try:
                await asyncio.Future()
            finally:
                await stopped.put(resident.capture_id)

        monkeypatch.setattr(worker.cli, "_run", fake_run)
        commands = worker.CommandInput()
        task = asyncio.create_task(worker.serve(SimpleNamespace(metrics=None), commands))
        commands.put(worker.Command(worker.CommandKind.LISTEN))
        assert await asyncio.wait_for(starts.get(), 1) == 1
        commands.put(worker.Command(worker.CommandKind.CANCEL, 99))
        commands.put(worker.Command(worker.CommandKind.CANCEL))
        commands.put(worker.Command(worker.CommandKind.CANCEL, 1))
        assert await asyncio.wait_for(stopped.get(), 1) == 1
        assert await asyncio.wait_for(starts.get(), 1) == 2
        commands.put(worker.Command(worker.CommandKind.CANCEL, 1))
        commands.put(worker.Command(worker.CommandKind.PAUSE))
        assert await asyncio.wait_for(stopped.get(), 1) == 2
        commands.put(worker.Command(worker.CommandKind.LISTEN))
        assert await asyncio.wait_for(starts.get(), 1) == 3
        commands.put(worker.Command(worker.CommandKind.QUIT))
        await asyncio.wait_for(task, 1)
        assert await stopped.get() == 3
        assert len(resets) == 3
        assert worker.cli.notice is original_notice

    asyncio.run(run())
    events = [json.loads(line) for line in capsys.readouterr().out.splitlines()]
    assert events[0]["phase"] == "loading"
    assert events[1]["phase"] == "paused"
    assert events[-1]["phase"] == "stopped"


def test_device_failure_stops_without_restart(monkeypatch, capsys):
    async def run():
        started = asyncio.Event()
        monkeypatch.setattr(
            worker, "Wake", lambda _: type("Wake", (), {"reset": lambda self: None})()
        )
        monkeypatch.setattr(worker, "desktop", lambda: object())

        async def fail(*args):
            started.set()
            raise OSError("microphone unavailable")

        monkeypatch.setattr(worker.cli, "_run", fail)
        commands = worker.CommandInput()
        task = asyncio.create_task(worker.serve(SimpleNamespace(metrics=None), commands))
        commands.put(worker.Command(worker.CommandKind.LISTEN))
        await asyncio.wait_for(started.wait(), 1)
        # The next command is processed only after the failed task has been reaped.
        commands.put(worker.Command(worker.CommandKind.QUIT))
        await asyncio.wait_for(task, 1)

    asyncio.run(run())
    events = [json.loads(line) for line in capsys.readouterr().out.splitlines()]
    assert any(e.get("message") == "microphone unavailable" for e in events)
