"""Owns loopback DevTools discovery and JSON RPC over its WebSocket."""

import asyncio
import contextlib
import http.client
import json
import re
import socket
import time
from collections.abc import Callable
from dataclasses import dataclass
from urllib.parse import urlsplit

from websockets.asyncio.client import connect
from websockets.exceptions import ConnectionClosed, WebSocketException

from .metrics import Metrics

MAX_MESSAGE = 1 << 20
DISCORD_HOST = "discord.com"


class CDPError(RuntimeError):
    """The local debugger refused, answered wrongly, or lost the connection."""

    def __init__(self, message: str, code: int | None = None):
        super().__init__(message)
        self.code = code


class CDPTimeout(CDPError):
    """The debugger did not answer within the deadline."""


class CDPDisconnected(CDPError):
    """The debugger connection closed before the call completed."""


@dataclass(frozen=True)
class Page:
    url: str
    websocket: str


def debug_port(port: object) -> int:
    if type(port) is not int or not 0 < port < 65536:
        raise ValueError("The Discord debug port must be between 1 and 65535.")
    return port


def fetch(port: int, path: str, timeout: float = 3.0) -> object:
    debug_port(port)
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        connection.request("GET", path, headers={"Host": f"127.0.0.1:{port}"})
        response = connection.getresponse()
        if response.status != 200:
            raise CDPError(f"Discord's debug port returned HTTP {response.status} for {path}.")
        body = response.read(MAX_MESSAGE + 1)
    except TimeoutError as error:
        raise CDPTimeout(f"Discord's debug port did not answer {path} in time.") from error
    except (OSError, http.client.HTTPException) as error:
        raise CDPError(f"Discord's debug port did not answer {path}.") from error
    finally:
        connection.close()
    if len(body) > MAX_MESSAGE:
        raise CDPError(f"Discord's debug port sent too much data for {path}.")
    try:
        return json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise CDPError(f"Discord's debug port sent invalid JSON for {path}.") from error


async def discover(
    port: int, metrics: Metrics, timeout: float = 3.0
) -> tuple[object, list[object]]:
    async def read(path: str, event: str) -> object:
        started = time.monotonic()
        try:
            value = await asyncio.to_thread(fetch, port, path, timeout)
        except CDPTimeout:
            with contextlib.suppress(OSError):
                await metrics.record(event, started, "timeout")
            raise
        except CDPError:
            with contextlib.suppress(OSError):
                await metrics.record(event, started, "error")
            raise
        with contextlib.suppress(OSError):
            await metrics.record(event, started, "ok")
        return value

    version = await read("/json/version", "cdp_http_version")
    if not isinstance(version, dict):
        raise CDPError("Discord's debug port returned an invalid version document.")
    targets = await read("/json/list", "cdp_http_targets")
    if not isinstance(targets, list):
        raise CDPError("Discord's debug port returned an invalid target list.")
    return version, targets


def select_page(port: int, targets: list[object]) -> Page:
    pages: list[tuple[str, str]] = []
    for row in targets:
        if not isinstance(row, dict):
            raise CDPError("Discord's debug port returned an invalid target.")
        kind, url, websocket = row.get("type"), row.get("url"), row.get("webSocketDebuggerUrl")
        if not all(isinstance(value, str) for value in (kind, url, websocket)):
            raise CDPError("Discord's debug port returned an invalid target.")
        if kind == "page":
            pages.append((url, websocket))
    if len(pages) != 1:
        raise CDPError("Discord's debug port does not expose exactly one page.")
    url, websocket = pages[0]
    page = urlsplit(url)
    if (
        page.scheme != "https"
        or page.hostname != DISCORD_HOST
        or page.port is not None
        or page.username
        or page.password
    ):
        raise CDPError("Discord's debug page is not the Discord web app.")
    socket = urlsplit(websocket)
    if (
        socket.scheme != "ws"
        or socket.hostname != "127.0.0.1"
        or socket.port != port
        or re.fullmatch(r"/devtools/page/[A-Za-z0-9_-]+", socket.path) is None
        or socket.query
        or socket.fragment
        or socket.username
        or socket.password
    ):
        raise CDPError("Discord's debug page did not offer a local debugger socket.")
    return Page(url, websocket)


class CDP:
    def __init__(self, websocket: str, metrics: Metrics, timeout: float = 3.0):
        self.websocket, self.metrics, self.timeout = websocket, metrics, timeout
        self._socket = None
        self._next = 0

    async def open(self) -> "CDP":
        started = time.monotonic()
        raw = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        raw.setblocking(False)
        try:
            port = urlsplit(self.websocket).port
            if port is None:
                raise CDPError("Discord debugger URL has no port.")
            async with asyncio.timeout(self.timeout):
                await asyncio.get_running_loop().sock_connect(raw, ("127.0.0.1", port))
            self._socket = await connect(
                self.websocket,
                sock=raw,
                proxy=None,
                open_timeout=self.timeout,
                close_timeout=1,
                max_size=MAX_MESSAGE,
                ping_interval=None,
            )
        except TimeoutError as error:
            raw.close()
            with contextlib.suppress(OSError):
                await self.metrics.record("cdp_open", started, "timeout")
            raise CDPTimeout("Connecting to Discord's local debugger timed out.") from error
        except (OSError, ValueError, WebSocketException) as error:
            raw.close()
            with contextlib.suppress(OSError):
                await self.metrics.record("cdp_open", started, "error")
            raise CDPDisconnected("Could not connect to Discord's local debugger.") from error
        except BaseException:
            raw.close()
            raise
        try:
            with contextlib.suppress(OSError):
                await self.metrics.record("cdp_open", started, "ok")
        except BaseException:
            await self.close()
            raise
        return self

    async def call(
        self,
        method: str,
        params: dict[str, object] | None = None,
        timeout: float | None = None,
        on_send: Callable[[], None] | None = None,
    ) -> dict[str, object]:
        if self._socket is None:
            raise CDPDisconnected("Discord's debugger is not connected.")
        self._next += 1
        ident, started, outcome = self._next, time.monotonic(), "error"
        try:
            async with asyncio.timeout(timeout or self.timeout):
                if on_send is not None:
                    on_send()
                await self._socket.send(
                    json.dumps({"id": ident, "method": method, "params": params or {}})
                )
                while True:
                    message = self._message(await self._socket.recv(), method)
                    if message.get("id") != ident:
                        continue
                    if error := message.get("error"):
                        code = error.get("code") if isinstance(error, dict) else None
                        if type(code) is not int:
                            raise CDPError(f"Discord's debugger rejected {method}.")
                        raise CDPError(f"Discord's debugger rejected {method} ({code}).", code)
                    result = message.get("result")
                    if not isinstance(result, dict):
                        raise CDPError(
                            f"Discord's debugger returned an invalid answer to {method}."
                        )
                    outcome = "ok"
                    return result
        except TimeoutError as error:
            outcome = "timeout"
            raise CDPTimeout(f"Discord's debugger did not answer {method} in time.") from error
        except (ConnectionClosed, OSError) as error:
            outcome = "disconnected"
            raise CDPDisconnected(f"Discord's debugger closed during {method}.") from error
        except asyncio.CancelledError:
            outcome = "cancelled"
            raise
        finally:
            with contextlib.suppress(OSError):
                await self.metrics.record(f"cdp_{method}", started, outcome)

    async def close(self) -> None:
        socket, self._socket = self._socket, None
        if socket is not None:
            await socket.close()

    @staticmethod
    def _message(raw: str | bytes, method: str) -> dict[str, object]:
        try:
            message = json.loads(raw)
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise CDPError(f"Discord's debugger sent invalid JSON during {method}.") from error
        if not isinstance(message, dict):
            raise CDPError(f"Discord's debugger sent an invalid message during {method}.")
        return message
