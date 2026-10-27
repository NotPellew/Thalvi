import asyncio

import httpx
import pytest

from thalvi_train.http_retry import post_with_retry


def post(handler: httpx.MockTransport, retries: int = 2) -> tuple[httpx.Response, list[float]]:
    delays: list[float] = []

    async def sleep(seconds: float) -> None:
        delays.append(seconds)

    async def go() -> httpx.Response:
        async with httpx.AsyncClient(transport=handler) as client:
            return await post_with_retry(
                client, "http://x.test/", json={}, retries=retries, base_delay=0.5, sleep=sleep
            )

    return asyncio.run(go()), delays


def test_retries_dropped_connections_and_throttling_with_doubling_delays() -> None:
    replies = iter([httpx.ConnectError("down"), httpx.Response(429), httpx.Response(200)])

    def handler(request: httpx.Request) -> httpx.Response:
        reply = next(replies)
        if isinstance(reply, Exception):
            raise reply
        return reply

    response, delays = post(httpx.MockTransport(handler))
    assert (response.status_code, delays) == (200, [0.5, 1.0])


def test_returns_the_last_failure_when_retries_run_out() -> None:
    response, delays = post(httpx.MockTransport(lambda _request: httpx.Response(503)))
    assert (response.status_code, delays) == (503, [0.5, 1.0])


def test_does_not_retry_client_errors() -> None:
    response, delays = post(httpx.MockTransport(lambda _request: httpx.Response(400)))
    assert (response.status_code, delays) == (400, [])


def test_raises_the_connection_error_when_retries_run_out() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("down")

    with pytest.raises(httpx.ConnectError):
        post(httpx.MockTransport(handler))
