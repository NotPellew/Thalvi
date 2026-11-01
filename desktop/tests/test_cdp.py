"""Exercises loopback discovery and the WebSocket JSON RPC boundary."""

import asyncio
import json
import threading
from contextlib import asynccontextmanager, contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest
from websockets.asyncio.server import serve

from thalvi_desktop.cdp import (
    CDP,
    CDPDisconnected,
    CDPError,
    CDPTimeout,
    discover,
    select_page,
)
from thalvi_desktop.metrics import Metrics

PAGE = {
    "type": "page",
    "url": "https://discord.com/channels/111/222",
    "webSocketDebuggerUrl": "ws://127.0.0.1:{port}/devtools/page/abc",
}
WORKER = {
    "type": "worker",
    "url": "",
    "webSocketDebuggerUrl": "ws://127.0.0.1:{port}/devtools/worker/1",
}


@contextmanager
def http_port(documents: dict[str, object], status: int = 200):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            body = json.dumps(documents.get(self.path, {})).encode()
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
    )
    thread.start()
    try:
        yield server.server_address[1]
    finally:
        server.shutdown()
        thread.join(timeout=2)
        server.server_close()


def page_for(port: int) -> dict[str, str]:
    return {
        key: value.format(port=port) if isinstance(value, str) else value
        for key, value in PAGE.items()
    }


def worker_for(port: int) -> dict[str, str]:
    return {
        key: value.format(port=port) if isinstance(value, str) else value
        for key, value in WORKER.items()
    }


def test_discovery_reads_the_version_and_one_discord_page():
    documents: dict[str, object] = {}

    async def behavior():
        with http_port(documents) as port:
            documents["/json/version"] = {"Browser": "Chrome/152"}
            documents["/json/list"] = [page_for(port), worker_for(port), worker_for(port)]
            version, targets = await discover(port, Metrics(None))
            selected = select_page(port, targets)
        assert version == {"Browser": "Chrome/152"}
        assert selected.url == PAGE["url"]
        assert selected.websocket == PAGE["webSocketDebuggerUrl"].format(port=port)

    asyncio.run(behavior())


@pytest.mark.parametrize(
    "targets,message",
    [
        ([], "exactly one page"),
        ([worker_for(1)], "exactly one page"),
        ([page_for(1), page_for(1)], "exactly one page"),
        (
            [{**page_for(1), "url": "https://discord.com.evil/channels/1/2"}],
            "not the Discord web app",
        ),
        (
            [{**page_for(1), "webSocketDebuggerUrl": "ws://10.0.0.1:1/devtools/page/a"}],
            "local debugger socket",
        ),
        (
            [{**page_for(1), "webSocketDebuggerUrl": "ws://127.0.0.1:2/devtools/page/a"}],
            "local debugger socket",
        ),
        ([{"type": "page", "url": 1, "webSocketDebuggerUrl": ""}], "invalid target"),
    ],
)
def test_page_selection_refuses_unknown_or_foreign_targets(targets, message):
    with pytest.raises(CDPError, match=message):
        select_page(1, targets)


@pytest.mark.parametrize(
    "documents,status,message",
    [
        ({}, 500, "HTTP 500"),
        ({"/json/version": "not an object"}, 200, "invalid version"),
        ({"/json/version": {}, "/json/list": {"rows": []}}, 200, "invalid target list"),
    ],
)
def test_discovery_refuses_bad_documents(documents, status, message):
    async def behavior():
        with http_port(documents, status) as port:
            await discover(port, Metrics(None))

    with pytest.raises(CDPError, match=message):
        asyncio.run(behavior())


def test_discovery_rejects_a_port_outside_the_loopback_range():
    with pytest.raises(ValueError, match="between 1 and 65535"):
        asyncio.run(discover(0, Metrics(None)))


@asynccontextmanager
async def rpc_server(handler):
    async with serve(handler, "127.0.0.1", 0) as listener:
        port = listener.sockets[0].getsockname()[1]
        yield f"ws://127.0.0.1:{port}"


def test_rpc_matches_ids_while_ignoring_interleaved_events():
    async def behavior():
        async def handler(socket):
            request = json.loads(await socket.recv())
            await socket.send('{"method":"Page.loadEventFired","params":{}}')
            await socket.send(json.dumps({"id": request["id"], "result": {"value": 7}}))
            await socket.wait_closed()

        async with rpc_server(handler) as url:
            cdp = await CDP(url, Metrics(None)).open()
            try:
                assert await cdp.call("Runtime.evaluate") == {"value": 7}
            finally:
                await cdp.close()

    asyncio.run(behavior())


def test_rpc_error_does_not_repeat_the_request_or_echo_the_message():
    async def behavior():
        requests = []

        async def handler(socket):
            request = json.loads(await socket.recv())
            requests.append(request)
            await socket.send(
                json.dumps(
                    {"id": request["id"], "error": {"code": -32000, "message": "secret content"}}
                )
            )
            await socket.wait_closed()

        async with rpc_server(handler) as url:
            cdp = await CDP(url, Metrics(None)).open()
            try:
                with pytest.raises(CDPError) as caught:
                    await cdp.call("Runtime.evaluate", {"expression": "secret"})
            finally:
                await cdp.close()
        assert caught.value.code == -32000
        assert "secret" not in str(caught.value)
        assert len(requests) == 1

    asyncio.run(behavior())


def test_rpc_disconnect_and_timeout_never_retry():
    async def behavior():
        async def disconnect(socket):
            json.loads(await socket.recv())
            await socket.close()

        async with rpc_server(disconnect) as url:
            cdp = await CDP(url, Metrics(None)).open()
            with pytest.raises(CDPDisconnected):
                await cdp.call("Runtime.evaluate")
            await cdp.close()

        release = asyncio.Event()

        async def slow(socket):
            json.loads(await socket.recv())
            await release.wait()

        async with rpc_server(slow) as url:
            cdp = await CDP(url, Metrics(None), timeout=0.05).open()
            with pytest.raises(CDPTimeout):
                await cdp.call("Runtime.evaluate")
            release.set()
            await cdp.close()

    asyncio.run(behavior())


def test_cancelling_a_call_does_not_send_another_request():
    async def behavior():
        received, release = asyncio.Event(), asyncio.Event()

        async def handler(socket):
            json.loads(await socket.recv())
            received.set()
            await release.wait()

        async with rpc_server(handler) as url:
            cdp = await CDP(url, Metrics(None)).open()
            task = asyncio.create_task(cdp.call("Runtime.evaluate"))
            await asyncio.wait_for(received.wait(), 1)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            await cdp.close()
            release.set()

    asyncio.run(behavior())


def test_metrics_record_each_call_at_completion_without_arguments(tmp_path):
    async def behavior():
        async def handler(socket):
            request = json.loads(await socket.recv())
            await socket.send(json.dumps({"id": request["id"], "result": {}}))
            await socket.wait_closed()

        async with rpc_server(handler) as url:
            path = tmp_path / "metrics.jsonl"
            cdp = await CDP(url, Metrics(path)).open()
            try:
                await cdp.call("Runtime.evaluate", {"expression": "secret text"})
            finally:
                await cdp.close()
            return path

    path = asyncio.run(behavior())
    events = [json.loads(line) for line in path.read_text().splitlines()]
    assert {event["event"] for event in events} == {"cdp_open", "cdp_Runtime.evaluate"}
    assert all(event["outcome"] == "ok" for event in events)
    assert "secret text" not in path.read_text()
    assert all(isinstance(event["total_ms"], int) for event in events)


def test_fetch_timeout_is_bounded_and_recorded(tmp_path):
    release = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            release.wait(timeout=2)
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"{}")

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
    )
    thread.start()
    path = tmp_path / "metrics.jsonl"
    try:
        with pytest.raises(CDPTimeout):
            asyncio.run(discover(server.server_address[1], Metrics(path), timeout=0.05))
    finally:
        release.set()
        server.shutdown()
        thread.join(timeout=2)
        server.server_close()
    events = [json.loads(line) for line in Path(path).read_text().splitlines()]
    assert events[0]["event"] == "cdp_http_version"
    assert events[0]["outcome"] == "timeout"


def test_websocket_redirect_never_reaches_another_loopback_listener():
    async def behavior():
        reached = asyncio.Event()

        async def second(socket):
            reached.set()
            await socket.wait_closed()

        async with serve(second, "127.0.0.1", 0) as target:
            port = target.sockets[0].getsockname()[1]

            def redirect(connection, request):
                response = connection.respond(302, "redirect")
                response.headers["Location"] = f"ws://127.0.0.1:{port}/devtools/page/other"
                return response

            async def unused(socket):
                await socket.wait_closed()

            async with serve(unused, "127.0.0.1", 0, process_request=redirect) as first:
                source = first.sockets[0].getsockname()[1]
                with pytest.raises(CDPError):
                    await CDP(f"ws://127.0.0.1:{source}/devtools/page/test", Metrics(None)).open()
                assert not reached.is_set()

    asyncio.run(behavior())


def test_cancellation_during_completion_metrics_does_not_start_a_later_call():
    async def behavior():
        received = []
        recording, release = asyncio.Event(), asyncio.Event()

        class BlockingMetrics:
            async def record(self, event, started, outcome):
                if event == "cdp_Runtime.evaluate":
                    recording.set()
                    await release.wait()

        async def handler(socket):
            async for message in socket:
                request = json.loads(message)
                received.append(request["method"])
                await socket.send(json.dumps({"id": request["id"], "result": {}}))

        async with rpc_server(handler) as url:
            client = await CDP(url, BlockingMetrics()).open()

            async def action():
                await client.call("Runtime.evaluate")
                await client.call("Input.insertText")

            pending = asyncio.create_task(action())
            await asyncio.wait_for(recording.wait(), 1)
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
            assert received == ["Runtime.evaluate"]
            await client.close()

    asyncio.run(behavior())
