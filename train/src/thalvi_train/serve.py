"""A streaming OpenAI-style chat endpoint for Glimmer with hearing: `python -m thalvi_train.serve`.

`POST /v1/chat/completions` takes `stream: true` requests whose user messages may hold
`input_audio` WAV parts, and streams Server-Sent Events: the reasoning as
`delta.reasoning_content`, the answer as `delta.content`, tool calls as `delta.tool_calls`,
a final chunk with `finish_reason`, an optional `usage` chunk, and `data: [DONE]`.

A request is rejected with an OpenAI-style 400 before any streaming starts. A failure
before the first token is a 500; after it the stream is just closed, since the client
reads `content` and an error must never be taken for the answer. Generation runs in a
worker thread, one request at a time, and stops after the token being decoded when the
client disconnects.
"""

import argparse
import asyncio
import json
import threading
import time
import uuid
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

from starlette.applications import Starlette
from starlette.concurrency import run_in_threadpool
from starlette.requests import Request
from starlette.responses import JSONResponse, Response, StreamingResponse
from starlette.routing import Route

from thalvi_train.chat import RequestError, parse_request
from thalvi_train.engine import Engine, Finished, Ready, Step, Update
from thalvi_train.features import DEFAULT_MODEL
from thalvi_train.reply import Content, Event, Reasoning

MODEL_ID = "muse-glimmer-30b-hears"
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


def create_app(engine: Engine, model: str = MODEL_ID) -> Starlette:
    """The ASGI app serving `engine`."""

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

    return Starlette(routes=[Route("/v1/chat/completions", chat_completions, methods=["POST"])])


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--glimmer", type=Path, required=True, help="Glimmer checkpoint")
    parser.add_argument("--projector", type=Path, required=True, help="run dir with the projector")
    parser.add_argument("--whisper-model", default=DEFAULT_MODEL)
    parser.add_argument("--device", choices=["cpu", "cuda"], default="cuda", help="the decoder's")
    parser.add_argument("--quantize", action="store_true", help="4-bit NF4 decoder; needs cuda")
    parser.add_argument("--encoder-device", choices=["cpu", "cuda"], default="cuda")
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
    return Engine(glimmer, tokenizer, projector, encoder, extractor)


def main(argv: list[str] | None = None) -> None:
    import uvicorn

    args = _parse_args(argv)
    if args.quantize and args.device != "cuda":
        raise SystemExit("--quantize needs --device cuda")
    uvicorn.run(create_app(load_engine(args)), host=args.host, port=args.port)


if __name__ == "__main__":
    main()
