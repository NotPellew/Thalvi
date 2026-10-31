"""Exercises capture, confirmation, task dispatch, and recovery as the caller sees them."""

import asyncio
import json
from pathlib import Path

import pytest

from thalvi_desktop.capture import Capture, Ending, Request
from thalvi_desktop.cli import replay
from thalvi_desktop.config import Config
from thalvi_desktop.discord import PreviewComposer
from thalvi_desktop.workflow import PreviewHost, Workflow


@pytest.fixture
def config():
    return Config.load(Path(__file__).parents[1] / "config.example.json")


@pytest.mark.parametrize(
    "ending,route",
    [("paste that", Ending.PASTE), ("SUBMIT!", Ending.SUBMIT), ("do that.", Ending.TASK)],
)
def test_capture_waits_through_pauses_and_dispatches_only_at_the_end(ending, route):
    capture = Capture()
    assert capture.final("Explain how to submit a change.") is None
    assert capture.final("Then show the next test.") is None
    request = capture.final(ending)
    assert request == Request(route, "Explain how to submit a change. Then show the next test.")
    with pytest.raises(ValueError, match="already finished"):
        capture.final("more")


def test_scratch_discards_only_the_latest_piece_and_cancel_discards_everything():
    capture = Capture()
    capture.final("Keep the first thought.")
    capture.final("Throw this away scratch that.")
    assert capture.text == "Keep the first thought."
    capture.final("Second thought.")
    capture.final("scratch that")
    assert capture.text == "Keep the first thought."
    assert capture.final("never mind") == Request(Ending.CANCEL, "")
    assert capture.text == ""


def test_configurable_endings_match_whole_phrases_only():
    capture = Capture({"send this": Ending.SUBMIT})
    assert capture.final("Resend this") is None
    assert capture.final("A literal send this inside the sentence") is None
    assert capture.final("Final words SEND THIS.") == Request(
        Ending.SUBMIT, "Resend this A literal send this inside the sentence Final words"
    )


def test_full_capture_preserves_the_text_already_accepted():
    capture = Capture()
    capture.final("x" * 19_990)
    with pytest.raises(ValueError, match="full"):
        capture.final("y" * 20)
    assert capture.final("paste that").text == "x" * 19_990


@pytest.mark.parametrize("confirmed", [False, True])
def test_enter_waits_for_confirmation_and_preserves_the_original_target(config, confirmed):
    async def behavior():
        host = PreviewHost(lambda _: None)
        target = host.capture()
        workflow = Workflow(config, host, lambda _: None)
        entered, release = asyncio.Event(), asyncio.Event()

        async def confirm():
            entered.set()
            await release.wait()
            return confirmed

        pending = asyncio.create_task(
            workflow.finish(Request(Ending.SUBMIT, "A prompt"), target, confirm)
        )
        await asyncio.wait_for(entered.wait(), 1)
        assert host.operations == []
        release.set()
        assert await asyncio.wait_for(pending, 1)
        assert host.operations == ([("deliver", "A prompt", True)] if confirmed else [])

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "phrase,kind",
    [
        ("open the project page", "open_url"),
        ("switch to T3 Code", "focus_app"),
        ("switch to Discord", "focus_app"),
    ],
)
def test_scripted_task_uses_the_typed_executor_and_unknown_tasks_keep_the_capture(
    config, phrase, kind
):
    async def behavior():
        host = PreviewHost(lambda _: None)
        workflow = Workflow(config, host, lambda _: None)

        async def no_confirmation():
            raise AssertionError("This action should not submit anything.")

        assert await workflow.finish(Request(Ending.TASK, phrase), host.capture(), no_confirmation)
        assert host.operations[0][0] == kind
        assert not await workflow.finish(
            Request(Ending.TASK, "an unknown task"), host.capture(), no_confirmation
        )
        assert host.operations[-1] == ("clipboard", "an unknown task", False)

    asyncio.run(behavior())


def test_replay_cli_runs_without_microphone_models_or_desktop_side_effects(
    config, tmp_path, capsys
):
    fixture = tmp_path / "flow.jsonl"
    fixture.write_text(
        "\n".join(
            json.dumps(event)
            for event in [
                {"wake": "capture"},
                {"final": "The first piece."},
                {"final": "The next piece submit"},
                {"confirm": True},
                {"wake": "capture"},
                {"final": "open the project page do that"},
                {"wake": "capture"},
                {"final": "never mind"},
            ]
        )
    )
    assert asyncio.run(replay(config, fixture, apply=False)) == 0
    output = capsys.readouterr().out
    assert "press Enter" in output
    assert "browser tab" in output
    assert "Discarded" in output


@pytest.mark.parametrize(
    "patch,message",
    [
        ({"wake": {"threshold": True}}, "threshold"),
        (
            {
                "scripts": [
                    {"phrase": "open", "action": "open_url", "destination": "javascript:alert(1)"}
                ]
            },
            "URL",
        ),
        ({"server": "ws://user:secret@localhost"}, "credentials"),
        ({"endings": {"enter": "submit"}}, "Configure"),
        ({"discord": {"debug_port": 0}}, "debug_port"),
        ({"discord": {"debug_port": 9223, "token": "x"}}, "Unknown"),
        (
            {"scripts": [{"phrase": "focus", "action": "focus_app", "destination": "missing"}]},
            "No app",
        ),
        ({"unknown": True}, "Unknown"),
    ],
)
def test_invalid_configuration_is_rejected_before_any_action(tmp_path, patch, message):
    path = tmp_path / "config.json"
    path.write_text(json.dumps(patch))
    with pytest.raises(ValueError, match=message):
        Config.load(path)


def test_discord_preview_shows_destination_and_body_then_cancel_keeps_the_draft(config):
    async def behavior():
        host = PreviewHost(lambda _: None)
        notices = []
        discord = PreviewComposer()
        workflow = Workflow(config, host, notices.append, discord=discord)

        async def confirm():
            two = "https://discord.com/channels/1566030981687677039/1566034064333340732"
            assert any(two in line for line in notices)
            assert any("Hello there!" in line for line in notices)
            return False

        assert await workflow.finish(
            Request(Ending.TASK, "message the demo channel saying Hello there!"),
            host.capture(),
            confirm,
        )
        assert host.operations == []
        assert discord.operations == []
        assert "draft remains in Discord" in notices[-1]

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "destination",
    [
        "https://discord.com/channels/123/456",
        "discord://discord.com/channels/@me/456",
    ],
)
def test_discord_destination_accepts_channels_and_dms(config, tmp_path, destination):
    path = tmp_path / "config.json"
    path.write_text(
        json.dumps(
            {
                "scripts": [
                    {"phrase": "message", "action": "discord_message", "destination": destination}
                ]
            }
        )
    )
    loaded = Config.load(path)
    assert loaded.scripts[0].destination == destination.replace("discord://", "https://")


@pytest.mark.parametrize(
    "destination",
    [
        "https://discord.com.evil/channels/123/456",
        "https://user@discord.com/channels/123/456",
        "https://discord.com/channels/123/456?token=x",
        "https://discord.com/channels/123/abc",
    ],
)
def test_discord_destination_rejects_other_urls(tmp_path, destination):
    path = tmp_path / "config.json"
    path.write_text(
        json.dumps(
            {
                "scripts": [
                    {"phrase": "message", "action": "discord_message", "destination": destination}
                ]
            }
        )
    )
    with pytest.raises(ValueError, match="Discord destination"):
        Config.load(path)


@pytest.mark.parametrize("confirmed", [False, True])
def test_replay_previews_discord_without_connecting_to_the_debugger(
    config, tmp_path, capsys, monkeypatch, confirmed
):
    import thalvi_desktop.cdp as cdp

    def unreachable(*args, **kwargs):
        raise AssertionError("Preview must not reach the debugger.")

    monkeypatch.setattr(cdp, "fetch", unreachable)
    fixture = tmp_path / "discord.jsonl"
    fixture.write_text(
        "\n".join(
            json.dumps(event)
            for event in [
                {"wake": "capture"},
                {"final": "message the demo channel saying Hello there do that"},
                {"confirm": confirmed},
            ]
        )
    )
    assert asyncio.run(replay(config, fixture, apply=False)) == 0
    output = capsys.readouterr().out
    assert (
        "Destination: https://discord.com/channels/1566030981687677039/1566034064333340732"
        in output
    )
    assert "Hello there" in output
    if confirmed:
        assert "Preview · would insert" in output
        assert "Submission attempted" not in output
    else:
        assert "draft remains in Discord" in output


def test_empty_discord_message_is_recovered_as_an_unconfigured_task(config):
    async def behavior():
        host = PreviewHost(lambda _: None)
        captured = Capture(config.endings).final("message the demo channel saying do that")

        async def confirm():
            raise AssertionError("An empty message cannot request a send.")

        assert not await Workflow(config, host, lambda _: None).finish(
            captured, host.capture(), confirm
        )
        assert host.operations == [("clipboard", "message the demo channel saying", False)]

    asyncio.run(behavior())


def test_cli_recovery_prints_text_when_clipboard_fails(capsys):
    from thalvi_desktop.cli import recover

    class BrokenClipboard:
        def copy(self, text):
            raise RuntimeError("clipboard failed")

    asyncio.run(recover(BrokenClipboard(), "Keep this"))
    assert "Keep this" in capsys.readouterr().out


@pytest.mark.parametrize(
    "spoken, expected",
    [
        ("open google.com do that", "https://google.com"),
        ("OPEN https://Example.com/Case?q=Value do that", "https://Example.com/Case?q=Value"),
    ],
)
def test_spoken_url_task_reaches_typed_executor(config, spoken, expected):
    async def behavior():
        host = PreviewHost(lambda _: None)
        request = Capture(config.endings).final(spoken)

        async def no_confirmation():
            raise AssertionError("Opening a page does not submit anything.")

        assert await Workflow(config, host, lambda _: None).finish(
            request, host.capture(), no_confirmation
        )
        assert host.operations == [("open_url", expected, False)]

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "spoken",
    [
        "open javascript:alert(1)",
        "open https://user@example.com",
        "open example.com extra",
        "open example.com;$(bad)",
        "open https://example.com:bad",
        "open https://example.com/%zz",
        "open https://example.com\\evil",
    ],
)
def test_invalid_url_task_is_preserved(config, spoken):
    async def behavior():
        host = PreviewHost(lambda _: None)

        async def no_confirmation():
            raise AssertionError("Invalid tasks do not request confirmation.")

        assert not await Workflow(config, host, lambda _: None).finish(
            Request(Ending.TASK, spoken), host.capture(), no_confirmation
        )
        assert host.operations == [("clipboard", spoken, False)]

    asyncio.run(behavior())
