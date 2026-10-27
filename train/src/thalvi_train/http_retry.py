"""Sends one HTTP request, retrying throttling, server errors and dropped connections."""

import asyncio
from collections.abc import Awaitable, Callable

import httpx

RETRYABLE_STATUS = frozenset({429, 500, 502, 503, 504})

Sleep = Callable[[float], Awaitable[None]]


async def post_with_retry(
    client: httpx.AsyncClient,
    url: str,
    *,
    json: object,
    headers: dict[str, str] | None = None,
    retries: int = 5,
    base_delay: float = 1.0,
    sleep: Sleep = asyncio.sleep,
) -> httpx.Response:
    """POSTs `json`, sleeping `base_delay * 2**attempt` between up to `retries` retries.

    Returns the first response that is not 429 or 5xx; the caller checks its status.
    The last failure is returned or raised once the retries are spent.
    """
    for attempt in range(retries + 1):
        try:
            response = await client.post(url, json=json, headers=headers)
        except httpx.TransportError:
            if attempt == retries:
                raise
        else:
            if response.status_code not in RETRYABLE_STATUS or attempt == retries:
                return response
        await sleep(base_delay * 2**attempt)
    raise AssertionError("unreachable")
