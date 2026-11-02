#!/usr/bin/env python3
"""Local stand-ins for Thalvi's chat model, speech synthesis and Meta realtime ASR.

Run: uv run --no-project --with websockets==17.4 python -I fake_endpoints.py \
         serve RUN_DIR HTTP_PORT ASR_PORT
     python3 -I fake_endpoints.py wav PATH

Every request and recognizer event is appended to RUN_DIR/fakes.jsonl.
Replies are scripted by the last user turn: "make it fail" answers HTTP 500, "long story"
streams about 15 s of text, "search for <words>" calls search_history, anything else echoes.
Speech turns are scripted: each line in RUN_DIR/asr-queue.txt becomes the transcript
of the next burst of microphone speech; with the queue empty, speech is ignored.
"""

import asyncio
import json
import math
import re
import struct
import sys
import threading
import time
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

RATE = 24000
VOICES = {"fake_heart": 330.0, "fake_bella": 523.0}
LOG_LOCK = threading.Lock()


class Log:
    def __init__(self, run_dir: Path):
        self.path = run_dir / "fakes.jsonl"

    def write(self, kind: str, **fields):
        line = json.dumps({"t": round(time.time(), 3), "kind": kind, **fields})
        with LOG_LOCK, self.path.open("a") as handle:
            handle.write(line + "\n")


def tone(seconds: float, freq: float) -> bytes:
    count = int(RATE * seconds)
    return b"".join(
        struct.pack("<h", int(6000 * math.sin(2 * math.pi * freq * i / RATE))) for i in range(count)
    )


def last_user_text(messages: list) -> str:
    for message in reversed(messages):
        if message.get("role") == "user":
            content = message.get("content")
            if isinstance(content, str):
                return content
            return "(audio turn)"
    return ""


def reply_plan(body: dict) -> dict:
    messages = body.get("messages", [])
    text = last_user_text(messages)
    lowered = text.lower()
    tool_results = [m for m in messages if m.get("role") == "tool"]
    if "make it fail" in lowered:
        return {"status": 500}
    if tool_results:
        found = tool_results[-1].get("content") or ""
        return {"words": f"Search done. The tool returned: {found[:160]}".split(), "delay": 0.03}
    match = re.search(r"search for (.+)", lowered)
    if match and body.get("tools"):
        query = re.sub(r"[^\w\s]", "", match.group(1)).strip()
        return {"tool": {"query": query, "scope": "all"}}
    if "long story" in lowered:
        sentence = "This is a long fake answer that keeps talking so you can stop it."
        return {"words": " ".join([sentence] * 12).split(), "delay": 0.12}
    return {"words": f"Fake reply to: {text.strip()}. That is all.".split(), "delay": 0.03}


def sse(chunk: dict) -> bytes:
    return f"data: {json.dumps(chunk)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    log: Log

    def log_message(self, format, *args):
        pass

    def read_json(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"{}")

    def head(self, status: int, content_type: str):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Connection", "close")
        self.end_headers()

    def do_GET(self):
        if self.path == "/health":
            self.head(200, "application/json")
            self.wfile.write(b'{"ok":true,"fake":"thalvi-verify"}')
        else:
            self.head(404, "application/json")
            self.wfile.write(b"{}")

    def do_POST(self):
        body = self.read_json()
        if self.path == "/v1/chat/completions":
            self.chat(body)
        elif self.path == "/v1/audio/speech":
            self.speech(body)
        else:
            self.head(404, "application/json")
            self.wfile.write(b"{}")

    def chat(self, body: dict):
        plan = reply_plan(body)
        self.log.write(
            "llm_request",
            user=last_user_text(body.get("messages", [])),
            tools=[t["function"]["name"] for t in body.get("tools", [])],
            tool_results=[
                m.get("content") for m in body.get("messages", []) if m.get("role") == "tool"
            ],
            plan="error" if "status" in plan else "tool" if "tool" in plan else "text",
        )
        if "status" in plan:
            self.head(plan["status"], "application/json")
            self.wfile.write(b'{"error":"fake failure"}')
            return
        self.head(200, "text/event-stream")
        try:
            if "tool" in plan:
                call = {
                    "index": 0,
                    "id": "call_fake_1",
                    "type": "function",
                    "function": {"name": "search_history", "arguments": json.dumps(plan["tool"])},
                }
                self.wfile.write(sse({"choices": [{"delta": {"tool_calls": [call]}}]}))
                self.wfile.write(sse({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}))
            else:
                for index, word in enumerate(plan["words"]):
                    piece = word if index == 0 else " " + word
                    self.wfile.write(sse({"choices": [{"delta": {"content": piece}}]}))
                    self.wfile.flush()
                    time.sleep(plan["delay"])
                self.wfile.write(sse({"choices": [{"delta": {}, "finish_reason": "stop"}]}))
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            self.log.write("llm_cancelled")

    def speech(self, body: dict):
        voice = body.get("voice", "")
        text = body.get("input", "")
        self.log.write("tts_request", voice=voice, input=text)
        if voice not in VOICES:
            self.head(400, "application/json")
            self.wfile.write(b'{"error":"unknown voice"}')
            return
        self.head(200, "application/octet-stream")
        pcm = tone(min(0.12 * len(text.split()), 6.0), VOICES[voice])
        try:
            for start in range(0, len(pcm), 4800):
                self.wfile.write(pcm[start : start + 4800])
                self.wfile.flush()
                time.sleep(0.02)
        except (BrokenPipeError, ConnectionResetError):
            self.log.write("tts_cancelled", input=text)


def rms(frame: bytes) -> float:
    count = len(frame) // 2
    if count == 0:
        return 0.0
    samples = struct.unpack(f"<{count}h", frame[: count * 2])
    return math.sqrt(sum(s * s for s in samples) / count)


def pop_queue(run_dir: Path) -> str | None:
    path = run_dir / "asr-queue.txt"
    if not path.exists():
        return None
    lines = [line for line in path.read_text().splitlines() if line.strip()]
    if not lines:
        return None
    path.write_text("".join(line + "\n" for line in lines[1:]))
    return lines[0].strip()


async def asr_connection(socket, run_dir: Path, log: Log):
    from websockets.exceptions import ConnectionClosed

    init = json.loads(await socket.recv())
    session = f"fake-asr-{int(time.time() * 1000)}"
    log.write(
        "asr_open", session=session, mode=init.get("mode"), encoding=init.get("audioEncoding")
    )
    await socket.send(json.dumps({"sessionId": session}))
    window = 480 * 2
    pending = b""
    loud_ms = quiet_ms = processed_ms = received = 0
    turn = 0
    transcript: str | None = None
    speaking = False
    try:
        async for message in socket:
            if isinstance(message, str):
                log.write("asr_text", session=session, message=message)
                continue
            received += len(message)
            pending += message
            while len(pending) >= window:
                frame, pending = pending[:window], pending[window:]
                processed_ms += 20
                loud = rms(frame) > 300
                if not speaking:
                    loud_ms = loud_ms + 20 if loud else 0
                    if loud_ms >= 120:
                        transcript = pop_queue(run_dir)
                        speaking = True
                        quiet_ms = 0
                        if transcript is None:
                            log.write("asr_ignored_speech", session=session, reason="queue empty")
                            continue
                        turn += 1
                        await socket.send(
                            json.dumps(
                                {
                                    "type": "speechStart",
                                    "turnId": str(turn),
                                    "audioProcessedMs": processed_ms,
                                }
                            )
                        )
                        log.write(
                            "asr_speech_start", session=session, turn=turn, bytes_received=received
                        )
                    continue
                quiet_ms = 0 if loud else quiet_ms + 20
                if transcript is not None and loud:
                    loud_ms += 20
                    if loud_ms % 300 == 0:
                        words = transcript.split()
                        shown = " ".join(words[: min(len(words), loud_ms // 300)])
                        await socket.send(
                            json.dumps({"type": "transcript", "transcript": shown, "final": False})
                        )
                if quiet_ms >= 600:
                    speaking = False
                    loud_ms = 0
                    if transcript is None:
                        continue
                    await socket.send(
                        json.dumps(
                            {
                                "type": "speechEnd",
                                "turnId": str(turn),
                                "audioProcessedMs": processed_ms,
                            }
                        )
                    )
                    await socket.send(
                        json.dumps(
                            {
                                "type": "speechComplete",
                                "turnId": str(turn),
                                "transcript": transcript,
                            }
                        )
                    )
                    log.write(
                        "asr_speech_complete", session=session, turn=turn, transcript=transcript
                    )
                    transcript = None
    except ConnectionClosed:
        pass
    log.write("asr_close", session=session, bytes_received=received)


async def serve_asr(run_dir: Path, port: int, log: Log):
    from websockets.asyncio.server import serve

    async with serve(
        lambda socket: asr_connection(socket, run_dir, log), "127.0.0.1", port, max_size=None
    ):
        await asyncio.Future()


def serve(run_dir: Path, http_port: int, asr_port: int):
    log = Log(run_dir)
    Handler.log = log
    http = ThreadingHTTPServer(("127.0.0.1", http_port), Handler)
    http.daemon_threads = True
    threading.Thread(target=http.serve_forever, daemon=True).start()
    log.write("fakes_ready", http_port=http_port, asr_port=asr_port)
    asyncio.run(serve_asr(run_dir, asr_port, log))


def write_wav(path: Path):
    rate = 48000
    samples = []
    for i in range(int(rate * 8.5)):
        t = i / rate
        if 1.0 <= t < 2.6:
            pitch = 140 + 60 * math.sin(2 * math.pi * 1.5 * t)
            envelope = 0.55 + 0.45 * math.sin(2 * math.pi * 4 * t)
            voice = sum(math.sin(2 * math.pi * pitch * k * t) / k for k in range(1, 6))
            samples.append(int(9000 * envelope * voice))
        else:
            samples.append(0)
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(
            struct.pack(f"<{len(samples)}h", *[max(-32767, min(32767, s)) for s in samples])
        )


if __name__ == "__main__":
    if sys.argv[1:2] == ["wav"]:
        write_wav(Path(sys.argv[2]))
    elif sys.argv[1:2] == ["serve"]:
        serve(Path(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]))
    else:
        sys.exit(__doc__)
