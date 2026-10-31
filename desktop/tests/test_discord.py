"""Exercises Discord webhook delivery through the workflow against a local fake API."""

import asyncio
import json
import threading
from dataclasses import replace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from thalvi_desktop.capture import Ending, Request
from thalvi_desktop.config import DEMO_CHANNEL, DEMO_GUILD, Config, WebhookSource
from thalvi_desktop.discord import Discord, Http, Rejected
from thalvi_desktop.tasks import DiscordMessage
from thalvi_desktop.workflow import PreviewHost, Workflow

WEBHOOK_ID = "123456789012345678"
TOKEN = "fake-webhook-token-not-a-real-discord-secret"
COMMAND = "message the demo channel saying Hello there!"
PROMPT = "Send this message to Thalvi Demo / #demo?\nHello there!"


@pytest.fixture
def config():
    return Config.load(Path(__file__).parents[1] / "config.example.json")


@pytest.fixture
def credential(monkeypatch):
    monkeypatch.setenv(
        "THALVI_DISCORD_WEBHOOK", f"https://discord.com/api/webhooks/{WEBHOOK_ID}/{TOKEN}"
    )


class FakeApi:
    def __init__(
        self,
        *,
        metadata=None,
        post_status=200,
        post_body=None,
        post_disconnect=False,
        post_redirect=None,
    ):
        self.requests = []
        self.metadata = (
            metadata
            if metadata is not None
            else {
                "id": WEBHOOK_ID,
                "type": 1,
                "guild_id": DEMO_GUILD,
                "channel_id": DEMO_CHANNEL,
            }
        )
        self.post_status = post_status
        self.post_body = (
            post_body if post_body is not None else {"id": "42", "channel_id": DEMO_CHANNEL}
        )
        self.post_disconnect = post_disconnect
        self.post_redirect = post_redirect

    def reply(self, handler, status, payload, location=None):
        data = json.dumps(payload).encode()
        handler.send_response(status)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(data)))
        if location is not None:
            handler.send_header("Location", location)
        handler.end_headers()
        handler.wfile.write(data)

    def __enter__(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                fake.requests.append(("GET", self.path, None))
                fake.reply(self, 200, fake.metadata)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = json.loads(self.rfile.read(length))
                fake.requests.append(("POST", self.path, body))
                if fake.post_disconnect:
                    self.close_connection = True
                    return
                if fake.post_redirect is not None:
                    fake.reply(self, fake.post_redirect, {}, "/stolen")
                    return
                fake.reply(self, fake.post_status, fake.post_body)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        return self

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(2)


def finish(config, api, answer=True, command=COMMAND):
    notices = []
    host = PreviewHost(lambda _: None)
    workflow = Workflow(config, host, notices.append, Discord(Http(api.base)))

    async def confirm():
        if answer is None:
            raise AssertionError("This action must fail before asking for confirmation.")
        assert any(PROMPT in notice for notice in notices)
        return answer

    result = asyncio.run(workflow.finish(Request(Ending.TASK, command), host.capture(), confirm))
    return result, host, notices


def test_confirmed_message_verifies_destination_sends_once_and_records_metrics(
    config, credential, tmp_path
):
    config = replace(config, metrics=tmp_path / "metrics.jsonl")
    with FakeApi() as api:
        result, host, notices = finish(config, api)
    assert result is True
    assert host.operations == []
    assert [request[0] for request in api.requests] == ["GET", "POST"]
    assert api.requests[0][1] == f"/webhooks/{WEBHOOK_ID}/{TOKEN}"
    assert api.requests[1][1] == f"/webhooks/{WEBHOOK_ID}/{TOKEN}?wait=true"
    assert api.requests[1][2] == {"content": "Hello there!", "allowed_mentions": {"parse": []}}
    assert notices[-1] == "Sent to Thalvi Demo / #demo."
    metrics = config.metrics.read_text()
    assert TOKEN not in metrics
    assert "Hello there!" not in metrics
    events = [json.loads(line) for line in metrics.splitlines()]
    assert {(event["event"], event["outcome"]) for event in events} >= {
        ("discord_webhook_check", "ok"),
        ("discord_webhook_send", "ok"),
    }
    assert all(isinstance(event["total_ms"], int) for event in events)


def test_cancel_never_contacts_discord(config, credential):
    with FakeApi() as api:
        result, host, notices = finish(config, api, answer=False)
    assert result is True
    assert api.requests == []
    assert host.operations == []
    assert notices[-1] == "Cancelled. Nothing was sent."


@pytest.mark.parametrize("field", ["guild_id", "channel_id"])
def test_wrong_webhook_destination_is_rejected_without_posting(config, credential, field):
    metadata = {
        "id": WEBHOOK_ID,
        "type": 1,
        "guild_id": DEMO_GUILD,
        "channel_id": DEMO_CHANNEL,
    }
    metadata[field] = "42"
    with FakeApi(metadata=metadata) as api:
        result, host, notices = finish(config, api)
    assert result is False
    assert [request[0] for request in api.requests] == ["GET"]
    assert any("does not point at Thalvi Demo / #demo" in notice for notice in notices)
    assert TOKEN not in "\n".join(notices)
    assert host.operations == [("clipboard", COMMAND, False)]


def test_http_rejection_keeps_the_text_and_never_claims_delivery(config, credential):
    with FakeApi(post_status=400) as api:
        result, host, notices = finish(config, api)
    assert result is False
    assert [request[0] for request in api.requests] == ["GET", "POST"]
    assert any("HTTP 400" in notice for notice in notices)
    assert not any("Sent to" in notice for notice in notices)
    assert host.operations == [("clipboard", COMMAND, False)]


def test_ambiguous_post_is_reported_and_never_retried(config, credential):
    with FakeApi(post_disconnect=True) as api:
        result, host, notices = finish(config, api)
    assert result is False
    assert [request[0] for request in api.requests] == ["GET", "POST"]
    assert any("Could not confirm whether Discord received" in notice for notice in notices)
    assert TOKEN not in "\n".join(notices)
    assert host.operations == [("clipboard", COMMAND, False)]


def test_redirect_is_not_followed_or_forwarded(config, credential):
    with FakeApi(post_redirect=307) as api:
        result, host, notices = finish(config, api)
    assert result is False
    assert [request[1] for request in api.requests] == [
        f"/webhooks/{WEBHOOK_ID}/{TOKEN}",
        f"/webhooks/{WEBHOOK_ID}/{TOKEN}?wait=true",
    ]
    assert any("HTTP 307" in notice for notice in notices)
    assert host.operations == [("clipboard", COMMAND, False)]


def test_oversized_message_fails_before_confirmation_or_posting(config, credential):
    command = "message the demo channel saying " + "x" * 2001
    with FakeApi() as api:
        result, host, notices = finish(config, api, answer=None, command=command)
    assert result is False
    assert api.requests == []
    assert any("limited to 2000 characters" in notice for notice in notices)
    assert host.operations == [("clipboard", command, False)]


def test_missing_credentials_fail_before_confirmation_and_posting(config, monkeypatch):
    monkeypatch.delenv("THALVI_DISCORD_WEBHOOK", raising=False)
    with FakeApi() as api:
        result, host, notices = finish(config, api, answer=None)
    assert result is False
    assert api.requests == []
    assert any("THALVI_DISCORD_WEBHOOK is not set" in notice for notice in notices)
    assert host.operations == [("clipboard", COMMAND, False)]


@pytest.mark.parametrize(
    "value",
    [
        f"https://evil.example/api/webhooks/{WEBHOOK_ID}/{TOKEN}",
        f"http://discord.com/api/webhooks/{WEBHOOK_ID}/{TOKEN}",
        f"https://discord.com:8443/api/webhooks/{WEBHOOK_ID}/{TOKEN}",
        f"https://discord.com/api/webhooks/{WEBHOOK_ID}/{TOKEN}?thread_id=1",
        f"https://discord.com/api/webhooks/{WEBHOOK_ID}/short",
        "not a url",
    ],
)
def test_webhook_url_must_be_a_plain_discord_incoming_webhook(config, monkeypatch, value):
    monkeypatch.setenv("THALVI_DISCORD_WEBHOOK", value)
    with FakeApi() as api:
        result, host, notices = finish(config, api, answer=None)
    assert result is False
    assert api.requests == []
    assert any("not a Discord incoming-webhook URL" in notice for notice in notices)
    assert TOKEN not in "\n".join(notices)
    assert host.operations == [("clipboard", COMMAND, False)]


def test_preview_confirms_without_credentials_or_network(config):
    with FakeApi() as api:
        notices = []
        host = PreviewHost(lambda _: None)
        workflow = Workflow(config, host, notices.append, Discord(Http(api.base)), preview=True)

        async def confirm():
            assert any(PROMPT in notice for notice in notices)
            return True

        result = asyncio.run(
            workflow.finish(Request(Ending.TASK, COMMAND), host.capture(), confirm)
        )
    assert result is True
    assert api.requests == []
    assert notices[-1] == "Preview · would send to Thalvi Demo / #demo; nothing was sent."


def test_private_webhook_file_is_required_and_read(tmp_path):
    path = tmp_path / "webhook.txt"
    path.write_text(f"https://discord.com/api/webhooks/{WEBHOOK_ID}/{TOKEN}\n", encoding="utf-8")
    path.chmod(0o600)
    task = DiscordMessage(
        DEMO_GUILD, DEMO_CHANNEL, "Thalvi Demo / #demo", "Hello", WebhookSource(file=path)
    )
    assert Discord().prepare(task).id == WEBHOOK_ID
    path.chmod(0o644)
    with pytest.raises(Rejected, match="chmod 600"):
        Discord().prepare(task)


@pytest.mark.parametrize("blocked", ["check", "send"])
def test_cancel_during_endpoint_preserves_text_and_never_starts_later_post(
    config, credential, tmp_path, blocked
):
    config = replace(config, metrics=tmp_path / "metrics.jsonl")
    entered, release, completed = threading.Event(), threading.Event(), threading.Event()
    posts = []

    class BlockingHttp:
        def get(self, path):
            if blocked == "check":
                entered.set()
                assert release.wait(2)
            return 200, json.dumps(
                {"id": WEBHOOK_ID, "guild_id": DEMO_GUILD, "channel_id": DEMO_CHANNEL}
            ).encode()

        def post(self, path, payload):
            posts.append(payload)
            entered.set()
            assert release.wait(2)
            return 200, json.dumps({"id": "42", "channel_id": DEMO_CHANNEL}).encode()

    class ObservableDiscord(Discord):
        def check(self, *args):
            try:
                return super().check(*args)
            finally:
                if blocked == "check":
                    completed.set()

        def send(self, *args):
            try:
                return super().send(*args)
            finally:
                if blocked == "send":
                    completed.set()

    async def behavior():
        notices = []
        host = PreviewHost(lambda _: None)
        workflow = Workflow(config, host, notices.append, ObservableDiscord(BlockingHttp()))

        async def confirm():
            return True

        pending = asyncio.create_task(
            workflow.finish(Request(Ending.TASK, COMMAND), host.capture(), confirm)
        )
        assert await asyncio.wait_for(asyncio.to_thread(entered.wait, 2), 3)
        pending.cancel()
        with pytest.raises(asyncio.CancelledError):
            await asyncio.wait_for(pending, 2)
        assert host.operations == [("clipboard", COMMAND, False)]
        if blocked == "send":
            assert any("may have completed" in message for message in notices)
        else:
            assert any("Nothing was sent" in message for message in notices)
        release.set()
        assert await asyncio.wait_for(asyncio.to_thread(completed.wait, 2), 3)
        assert len(posts) == (1 if blocked == "send" else 0)
        events = [json.loads(line) for line in config.metrics.read_text().splitlines()]
        assert any(
            event["event"] == "desktop_task" and event["outcome"] == "interrupted"
            for event in events
        )
        assert any(event["event"] == f"discord_webhook_{blocked}" for event in events)

    asyncio.run(behavior())


def test_endpoint_metrics_measure_each_call_at_completion(
    config, credential, tmp_path, monkeypatch
):
    from types import SimpleNamespace

    import thalvi_desktop.discord as discord_module

    clock = SimpleNamespace(value=0.0)
    monkeypatch.setattr(discord_module, "time", SimpleNamespace(monotonic=lambda: clock.value))
    config = replace(config, metrics=tmp_path / "metrics.jsonl")

    class TimedHttp:
        def get(self, path):
            clock.value += 0.25
            return 200, json.dumps(
                {"id": WEBHOOK_ID, "guild_id": DEMO_GUILD, "channel_id": DEMO_CHANNEL}
            ).encode()

        def post(self, path, payload):
            clock.value += 0.75
            return 200, json.dumps({"id": "42", "channel_id": DEMO_CHANNEL}).encode()

    async def behavior():
        host = PreviewHost(lambda _: None)
        workflow = Workflow(config, host, lambda _: None, Discord(TimedHttp()))

        async def confirm():
            return True

        assert await workflow.finish(Request(Ending.TASK, COMMAND), host.capture(), confirm)

    # Both modules normally share time.monotonic; keep measurement clock consistent.
    import thalvi_desktop.workflow as workflow_module

    monkeypatch.setattr(workflow_module, "time", SimpleNamespace(monotonic=lambda: clock.value))
    asyncio.run(behavior())
    events = {e["event"]: e for e in map(json.loads, config.metrics.read_text().splitlines())}
    assert events["discord_webhook_check"]["total_ms"] == 250
    assert events["discord_webhook_send"]["total_ms"] == 750


def test_cancelled_queued_post_never_starts(config, credential, monkeypatch):
    async def behavior():
        queued, release = asyncio.Event(), asyncio.Event()
        workers, posts = [], []

        class Http:
            def get(self, path):
                return 200, json.dumps(
                    {"id": WEBHOOK_ID, "guild_id": DEMO_GUILD, "channel_id": DEMO_CHANNEL}
                ).encode()

            def post(self, path, payload):
                posts.append(payload)
                return 200, json.dumps({"id": "42", "channel_id": DEMO_CHANNEL}).encode()

        discord = Discord(Http())
        to_thread = asyncio.to_thread

        async def schedule(operation, *args):
            if operation == discord.send:
                workers.append(asyncio.current_task())
                queued.set()
                await release.wait()
            return await to_thread(operation, *args)

        monkeypatch.setattr(asyncio, "to_thread", schedule)
        host = PreviewHost(lambda _: None)
        workflow = Workflow(config, host, lambda _: None, discord)

        async def confirm():
            return True

        pending = asyncio.create_task(
            workflow.finish(Request(Ending.TASK, COMMAND), host.capture(), confirm)
        )
        await asyncio.wait_for(queued.wait(), 2)
        pending.cancel()
        with pytest.raises(asyncio.CancelledError):
            await asyncio.wait_for(pending, 2)
        release.set()
        await asyncio.wait_for(asyncio.gather(*workers, return_exceptions=True), 2)
        assert posts == []
        assert host.operations == [("clipboard", COMMAND, False)]

    asyncio.run(behavior())


@pytest.mark.parametrize("method", ["GET", "POST"])
def test_invalid_http_response_is_sanitized_and_preserves_text(config, credential, method):
    import http.client

    class BrokenOpener:
        def open(self, request, timeout):
            if request.get_method() == method:
                raise http.client.BadStatusLine(f"secret {TOKEN}")
            return Response()

    class Response:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

        def read(self, size):
            return json.dumps(
                {"id": WEBHOOK_ID, "guild_id": DEMO_GUILD, "channel_id": DEMO_CHANNEL}
            ).encode()

    transport = Http()
    transport.opener = BrokenOpener()
    notices = []
    host = PreviewHost(lambda _: None)
    workflow = Workflow(config, host, notices.append, Discord(transport))

    async def confirm():
        return True

    assert not asyncio.run(workflow.finish(Request(Ending.TASK, COMMAND), host.capture(), confirm))
    assert host.operations == [("clipboard", COMMAND, False)]
    assert TOKEN not in " ".join(notices)
    assert any(
        ("Nothing was sent" if method == "GET" else "Check the channel") in x for x in notices
    )
