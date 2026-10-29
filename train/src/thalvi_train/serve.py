"""A streaming OpenAI-style chat endpoint for Glimmer with hearing: `python -m thalvi_train.serve`.

`POST /v1/chat/completions` takes `stream: true` requests whose user messages may hold
`input_audio` WAV parts, and streams Server-Sent Events: the reasoning as
`delta.reasoning_content`, the answer as `delta.content`, tool calls as `delta.tool_calls`,
a final chunk with `finish_reason`, an optional `usage` chunk, and `data: [DONE]`.
A spoken turn without tools is answered in echo mode unless `--reply-mode reasoning`:
the reasoning is one line saying what Glimmer heard, so the answer starts sooner.

With an API key (`--api-key` or `THALVI_ENDPOINT_KEY`) every route demands
`Authorization: Bearer <key>` and answers 401 otherwise. `GET /models` and `GET /v1/models`
list the one model.

A request is rejected with an OpenAI-style 400 before any streaming starts. A failure
before the first token is a 500; after it the stream is just closed, since the client
reads `content` and an error must never be taken for the answer. Generation runs in a
worker thread, one request at a time, and stops after the token being decoded when the
client disconnects.

On an unquantized CUDA decoder the model is loaded with a CUDA-graph decoder of
`--static-length` positions (`thalvi_train.static_decode`), compiled and captured once at
load; a request too long for it, or one that arrives while it is busy, decodes the stock way.
"""

import argparse
import asyncio
import hmac
import json
import os
import threading
import time
import uuid
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any, get_args

from starlette.applications import Starlette
from starlette.concurrency import run_in_threadpool
from starlette.requests import Request
from starlette.responses import JSONResponse, Response, StreamingResponse
from starlette.routing import Route
from starlette.types import ASGIApp, Receive, Scope, Send

from thalvi_train.chat import RequestError, parse_request
from thalvi_train.echo import ReplyMode
from thalvi_train.engine import Engine, Finished, Ready, Step, Update
from thalvi_train.features import DEFAULT_MODEL
from thalvi_train.reply import Content, Event, Reasoning
from thalvi_train.static_decode import DEFAULT_CAPACITY, StaticDecoder

MODEL_ID = "muse-glimmer-30b-hears"
API_KEY_ENV = "THALVI_ENDPOINT_KEY"
DONE = "data: [DONE]\n\n"
_END = object()


class CompletionWriter:
    """Builds the SSE frames of one streamed completion."""

    def __init__(self, model: str = MODEL_ID) -> None:
        self._id = f"chatcmpl-{uuid.uuid4().hex}"
        self._created = int(time.time())
        self._model = model
        self._tool_calls = 0

    def _frame(self, choices: list[dict[str, Any]], **extra: Any) -> str:
        chunk = {
            "id": self._id,
            "object": "chat.completion.chunk",
            "created": self._created,
            "model": self._model,
            "choices": choices,
            **extra,
        }
        return f"data: {json.dumps(chunk, ensure_ascii=False, separators=(',', ':'))}\n\n"

    def _delta(self, delta: dict[str, Any], finish_reason: str | None = None) -> str:
        return self._frame([{"index": 0, "delta": delta, "finish_reason": finish_reason}])

    def opening(self) -> str:
        """The frame that announces the assistant role."""
        return self._delta({"role": "assistant"})

    def event(self, event: Event) -> str:
        """The frame of one reasoning piece, answer piece or complete tool call."""
        if isinstance(event, Reasoning):
            return self._delta({"reasoning_content": event.text})
        if isinstance(event, Content):
            return self._delta({"content": event.text})
        index, self._tool_calls = self._tool_calls, self._tool_calls + 1
        function = {"name": event.name, "arguments": event.arguments_json()}
        entry = {
            "index": index,
            "id": f"call_{uuid.uuid4().hex[:24]}",
            "type": "function",
            "function": function,
        }
        return self._delta({"tool_calls": [entry]})

    def closing(self, finished: Finished, *, include_usage: bool) -> str:
        """The frame with `finish_reason`, the usage frame if wanted, and `[DONE]`."""
        frames = self._delta({}, finished.reason)
        if include_usage:
            usage = {
                "prompt_tokens": finished.prompt_tokens,
                "completion_tokens": finished.completion_tokens,
                "total_tokens": finished.prompt_tokens + finished.completion_tokens,
            }
            frames += self._frame([], usage=usage)
        return frames + DONE


def _error(status: int, message: str, kind: str) -> JSONResponse:
    body = {"error": {"message": message, "type": kind, "param": None, "code": None}}
    return JSONResponse(body, status_code=status)


class BearerAuth:
    """Answers 401 to every HTTP request that lacks `Authorization: Bearer <api_key>`."""

    def __init__(self, app: ASGIApp, api_key: str) -> None:
        self._app = app
        self._expected = f"Bearer {api_key}".encode()

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] == "http":
            given = dict(scope["headers"]).get(b"authorization", b"")
            if not hmac.compare_digest(given, self._expected):
                response = _error(401, "missing or invalid API key", "invalid_request_error")
                response.headers["WWW-Authenticate"] = "Bearer"
                await response(scope, receive, send)
                return
        await self._app(scope, receive, send)


def create_app(engine: Engine, model: str = MODEL_ID, api_key: str | None = None) -> Starlette:
    """The ASGI app serving `engine`; with a non-empty `api_key` every route needs it."""

    async def models(request: Request) -> Response:
        entry = {"id": model, "object": "model", "created": 0, "owned_by": "thalvi"}
        return JSONResponse({"object": "list", "data": [entry]})

    async def chat_completions(request: Request) -> Response:
        try:
            body = await request.json()
        except ValueError:
            return JSONResponse(RequestError("the body is not valid JSON").body(), 400)
        try:
            chat = await run_in_threadpool(parse_request, body)
        except RequestError as error:
            return JSONResponse(error.body(), 400)

        loop = asyncio.get_running_loop()
        updates: asyncio.Queue[Any] = asyncio.Queue()
        cancelled = threading.Event()

        def emit(update: object) -> None:
            loop.call_soon_threadsafe(updates.put_nowait, update)

        def work() -> None:
            try:
                engine.run(chat, emit, cancelled)
            except Exception as error:
                emit(error)
            finally:
                emit(_END)

        worker = loop.run_in_executor(None, work)

        async def stream() -> AsyncIterator[str]:
            writer = CompletionWriter(model)
            try:
                yield writer.opening()
                while True:
                    update: Update | Exception | object = await updates.get()
                    if update is _END or isinstance(update, Exception):
                        return
                    if await request.is_disconnected():
                        return
                    if isinstance(update, Step):
                        for event in update.events:
                            yield writer.event(event)
                    elif isinstance(update, Finished):
                        yield writer.closing(update, include_usage=chat.include_usage)
                        return
            finally:
                cancelled.set()
                await worker

        try:
            first = await updates.get()
        except BaseException:
            cancelled.set()
            raise
        if not isinstance(first, Ready):
            cancelled.set()
            await worker
            message = str(first) if isinstance(first, Exception) else "generation did not start"
            return _error(500, message, "server_error")
        return StreamingResponse(
            stream(),
            media_type="text/event-stream",
            headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
        )

    routes = [
        Route("/v1/chat/completions", chat_completions, methods=["POST"]),
        Route("/models", models, methods=["GET"]),
        Route("/v1/models", models, methods=["GET"]),
    ]
    app = Starlette(routes=routes)
    if api_key:
        app.add_middleware(BearerAuth, api_key=api_key)
    return app


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--glimmer", type=Path, required=True, help="Glimmer checkpoint")
    parser.add_argument("--projector", type=Path, required=True, help="run dir with the projector")
    parser.add_argument("--whisper-model", default=DEFAULT_MODEL)
    parser.add_argument("--device", choices=["cpu", "cuda"], default="cuda", help="the decoder's")
    parser.add_argument("--quantize", action="store_true", help="4-bit NF4 decoder; needs cuda")
    parser.add_argument("--encoder-device", choices=["cpu", "cuda"], default="cuda")
    parser.add_argument(
        "--static-length",
        type=int,
        default=DEFAULT_CAPACITY,
        help="KV positions of the CUDA-graph decoder, 0 for none; ignored when quantized",
    )
    parser.add_argument(
        "--reply-mode",
        choices=get_args(ReplyMode),
        default="echo",
        help="echo: a spoken turn is answered after a one-line echo; reasoning: full reasoning",
    )
    parser.add_argument(
        "--api-key",
        default=os.environ.get(API_KEY_ENV),
        help=f"require this bearer key on every route (default: ${API_KEY_ENV}; unset: no auth)",
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8090)
    return parser.parse_args(argv)


def load_engine(args: argparse.Namespace) -> Engine:
    """Loads every model the arguments name onto its device."""
    import torch
    from transformers import AutoTokenizer

    from thalvi_train.evaluate import load_projector
    from thalvi_train.features import load_encoder
    from thalvi_train.glimmer import load_glimmer
    from thalvi_train.prompts import GlimmerTokenizer

    glimmer = load_glimmer(args.glimmer, device=args.device, quantize=args.quantize)
    projector, config = load_projector(args.projector, glimmer.hidden_size, glimmer.device)
    if glimmer.device.type == "cuda":
        projector = projector.to(torch.bfloat16)
    encoder, extractor = load_encoder(args.whisper_model, args.encoder_device, config.encoder_dim)
    tokenizer = GlimmerTokenizer(AutoTokenizer.from_pretrained(args.glimmer))
    if glimmer.device.type == "cuda" and not args.quantize and args.static_length > 0:
        started = time.monotonic()
        glimmer.static = StaticDecoder(glimmer, args.static_length, compile=True)
        glimmer.static.warm_up()
        print(
            f"[static] decoder graph captured for {args.static_length} positions "
            f"in {time.monotonic() - started:.1f} s",
            flush=True,
        )
    return Engine(glimmer, tokenizer, projector, encoder, extractor, args.reply_mode)


def main(argv: list[str] | None = None) -> None:
    import uvicorn

    args = _parse_args(argv)
    if args.quantize and args.device != "cuda":
        raise SystemExit("--quantize needs --device cuda")
    uvicorn.run(create_app(load_engine(args), api_key=args.api_key), host=args.host, port=args.port)


if __name__ == "__main__":
    main()
