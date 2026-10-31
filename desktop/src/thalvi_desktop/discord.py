"""Owns the narrow Discord incoming-webhook delivery to the demo channel."""

import http.client
import json
import os
import re
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from urllib.parse import urlsplit

from .config import WebhookSource
from .tasks import DiscordMessage

API = "https://discord.com/api/v10"
MAXIMUM_MESSAGE = 2000
MAXIMUM_SECRET = 4096
TIMEOUT = 10
WEBHOOK_PATH = re.compile(r"/api/webhooks/(?P<id>\d{17,20})/(?P<token>[A-Za-z0-9._-]{20,200})")

Record = Callable[[str, float, str], None]


class WebhookError(RuntimeError):
    """Delivery failed or its outcome is uncertain; the workflow keeps the captured text."""


class Rejected(WebhookError):
    """The setup or Discord refused the message; nothing was posted."""


class Uncertain(WebhookError):
    """The message may have reached Discord; never retry automatically."""


@dataclass(frozen=True)
class Webhook:
    id: str
    token: str = field(repr=False)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Http:
    def __init__(self, base: str = API):
        self.base = base.rstrip("/")
        self.opener = urllib.request.build_opener(_NoRedirect())

    def get(self, path: str) -> tuple[int, bytes]:
        return self._send(urllib.request.Request(self.base + path, method="GET"))

    def post(self, path: str, payload: dict[str, object]) -> tuple[int, bytes]:
        request = urllib.request.Request(
            self.base + path,
            data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        return self._send(request)

    def _send(self, request: urllib.request.Request) -> tuple[int, bytes]:
        request.add_header("User-Agent", "ThalviDesktop/0.1")
        request.add_header("Accept", "application/json")
        try:
            try:
                with self.opener.open(request, timeout=TIMEOUT) as response:
                    return response.status, response.read(1_000_001)
            except urllib.error.HTTPError as error:
                return error.code, error.read(1_000_001)
        except (OSError, http.client.HTTPException) as error:
            raise ConnectionError("The Discord API could not be reached.") from error


def _object(body: bytes) -> dict[str, object] | None:
    try:
        value = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return None
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        return None
    return value


def _secret(source: WebhookSource) -> str:
    if source.env is not None:
        value = os.environ.get(source.env)
        if value is None or not value.strip():
            raise Rejected(f"{source.env} is not set. Nothing was sent.")
        return value.strip()
    if source.file is None:
        raise Rejected("No Discord webhook credential is configured. Nothing was sent.")
    path = source.file
    try:
        if not path.is_file():
            raise Rejected("The Discord webhook file does not exist. Nothing was sent.")
        if path.stat().st_mode & 0o077:
            raise Rejected(
                "The Discord webhook file is readable by other users. Run chmod 600 on it. "
                "Nothing was sent."
            )
        with path.open(encoding="utf-8") as stream:
            value = stream.read(MAXIMUM_SECRET + 1)
    except FileNotFoundError:
        raise Rejected("The Discord webhook file does not exist. Nothing was sent.") from None
    except OSError as error:
        raise Rejected("The Discord webhook file could not be read. Nothing was sent.") from error
    if len(value) > MAXIMUM_SECRET:
        raise Rejected("The Discord webhook file is too large. Nothing was sent.")
    if not value.strip():
        raise Rejected("The Discord webhook file is empty. Nothing was sent.")
    return value.strip()


def _webhook(value: str) -> Webhook:
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError:
        parsed, port = None, None
    match = WEBHOOK_PATH.fullmatch(parsed.path) if parsed is not None else None
    if (
        parsed is None
        or parsed.scheme != "https"
        or parsed.hostname != "discord.com"
        or port is not None
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or match is None
    ):
        raise Rejected(
            "The Discord webhook value is not a Discord incoming-webhook URL. Nothing was sent."
        )
    return Webhook(match["id"], match["token"])


class Discord:
    def __init__(self, http: Http | None = None):
        self.http = http or Http()

    def validate(self, task: DiscordMessage) -> None:
        if not task.message.strip():
            raise Rejected("The Discord message is empty. Nothing was sent.")
        if len(task.message) > MAXIMUM_MESSAGE:
            raise Rejected(
                f"Discord messages are limited to {MAXIMUM_MESSAGE} characters. Nothing was sent."
            )

    def prepare(self, task: DiscordMessage) -> Webhook:
        self.validate(task)
        return _webhook(_secret(task.webhook))

    def check(self, webhook: Webhook, task: DiscordMessage, record: Record) -> None:
        path = f"/webhooks/{webhook.id}/{webhook.token}"
        started = time.monotonic()
        try:
            status, body = self.http.get(path)
        except OSError as error:
            record("discord_webhook_check", started, "uncertain")
            raise Uncertain(
                "Could not verify the Discord webhook destination. Nothing was sent."
            ) from error
        if status != 200:
            record("discord_webhook_check", started, "rejected")
            raise Rejected("Discord did not confirm the webhook destination. Nothing was sent.")
        metadata = _object(body)
        if (
            metadata is None
            or str(metadata.get("id")) != webhook.id
            or str(metadata.get("guild_id")) != task.guild
            or str(metadata.get("channel_id")) != task.channel
        ):
            record("discord_webhook_check", started, "rejected")
            raise Rejected(f"The Discord webhook does not point at {task.label}. Nothing was sent.")
        record("discord_webhook_check", started, "ok")

    def send(self, webhook: Webhook, task: DiscordMessage, record: Record) -> None:
        path = f"/webhooks/{webhook.id}/{webhook.token}"
        started = time.monotonic()
        try:
            status, body = self.http.post(
                path + "?wait=true",
                {"content": task.message, "allowed_mentions": {"parse": []}},
            )
        except OSError as error:
            record("discord_webhook_send", started, "uncertain")
            raise Uncertain(
                "Could not confirm whether Discord received the message. Check the channel "
                "before retrying; nothing was retried."
            ) from error
        if not 200 <= status < 300:
            uncertain = status >= 500 or status == 408
            record("discord_webhook_send", started, "uncertain" if uncertain else "rejected")
            if uncertain:
                raise Uncertain(
                    "The Discord API did not confirm the message. Check the channel before "
                    "retrying; nothing was retried."
                )
            raise Rejected(f"Discord rejected the message (HTTP {status}). Nothing was sent.")
        receipt = _object(body)
        if (
            receipt is None
            or not isinstance(receipt.get("id"), str)
            or str(receipt.get("channel_id")) != task.channel
        ):
            record("discord_webhook_send", started, "uncertain")
            raise Uncertain(
                "Discord accepted the request but returned no verifiable receipt. Check the "
                "channel before retrying; nothing was retried."
            )
        record("discord_webhook_send", started, "ok")
