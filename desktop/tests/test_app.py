"""Tests the resident launcher and worker protocol without native devices."""

import io
import json
import subprocess
import sys
from pathlib import Path
from unittest.mock import Mock

from thalvi_desktop import app
from thalvi_desktop.gtk_app import Worker


def test_launcher_uses_system_gtk_and_backend_python(monkeypatch, tmp_path):
    call = Mock(return_value=7)
    monkeypatch.setattr(subprocess, "call", call)
    assert app.launch(tmp_path / "config.json") == 7
    args = call.call_args.args[0]
    assert args == [
        "/usr/bin/python3",
        str(Path(app.__file__).with_name("gtk_app.py")),
        sys.executable,
        str(tmp_path / "config.json"),
    ]


def test_worker_state_commands_and_old_generation():
    changed = Mock()
    worker = Worker("python", "/config", None, changed)
    process = Mock(stdin=io.BytesIO())
    worker.process = process
    worker.receive(
        process, {"type": "state", "phase": "pending", "message": "Confirm", "capture_id": 3}
    )
    worker.command("cancel")
    assert json.loads(process.stdin.getvalue()) == {"command": "cancel", "capture_id": 3}
    worker.receive(Mock(), {"type": "state", "phase": "capturing", "message": "old"})
    assert worker.phase == "pending"
    worker.receive(process, {"type": "notice", "message": "private preview"})
    assert worker.preview == "private preview"
    worker.exited(process, False)
    assert worker.phase == "error"
    assert worker.process is None


def test_delivery_remains_cancelable_with_its_capture_id():
    worker = Worker("python", "/config", None, Mock())
    process = Mock(stdin=io.BytesIO())
    worker.process = process
    worker.receive(
        process, {"type": "state", "phase": "working", "message": "Typing", "capture_id": 7}
    )
    worker.command("cancel")
    assert json.loads(process.stdin.getvalue()) == {"command": "cancel", "capture_id": 7}


def test_invalid_protocol_reaps_worker():
    process = Mock(stdout=io.BytesIO(b'{"type":"state","phase":"bogus","message":"x"}\n'))
    worker = Worker("python", "/config", lambda fn, *args: fn(*args), Mock())
    worker.process = process
    worker.stop_process = Mock()
    worker.read(process)
    worker.stop_process.assert_called_once_with(process)
    assert worker.phase == "error"
    assert "invalid response" in worker.message


def test_pending_notification_prompt_contains_action_and_target_only():
    from thalvi_desktop.gtk_app import pending_prompt

    prompt = pending_prompt(
        {"ending": "submit", "target": "Firefox", "message": "private transcript"}, 2.5
    )
    assert prompt == (
        "Confirm submit to Firefox: pause 2.5 seconds to send. Keep speaking or Cancel to continue."
    )
    assert "private transcript" not in prompt
