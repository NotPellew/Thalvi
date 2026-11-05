"""The recognizer protocol, turn events and resampling, driven through a fake model."""

import asyncio
import json

import numpy as np
import pytest
from websockets.asyncio.client import connect
from websockets.asyncio.server import serve

from thalvi_asr import INPUT_RATE, MODEL_RATE, STEP_SAMPLES, Resampler, Server, Step, Turns, handshake_error

HELLO = {
    "authorization": {"accessToken": "Bearer x"},
    "audioEncoding": "PCM_24KHZ",
    "model": "m",
    "mode": "ENDPOINTING",
}
# 24 kHz bytes that make exactly one 0.16 s model step.
ONE_STEP = np.zeros(STEP_SAMPLES * INPUT_RATE // MODEL_RATE, "<i2").tobytes()
WAIT_S = 5


class FakeStream:
    def __init__(self, steps: list[Step]) -> None:
        self.steps = steps
        self.calls: list[tuple[int, bool]] = []

    def step(self, samples: np.ndarray, last: bool) -> Step:
        assert samples.shape == (STEP_SAMPLES,)
        self.calls.append((len(self.calls), last))
        return self.steps[len(self.calls) - 1] if len(self.calls) <= len(self.steps) else Step("", "")


class FakeModel:
    def __init__(self, steps: list[Step]) -> None:
        self.streams: list[FakeStream] = []
        self.steps = steps

    def open(self) -> FakeStream:
        self.streams.append(FakeStream(self.steps))
        return self.streams[-1]


async def session(model: FakeModel, script) -> list[dict]:
    server = Server(model)
    async with serve(server.handle, "127.0.0.1", 0) as running:
        port = running.sockets[0].getsockname()[1]
        async with connect(f"ws://127.0.0.1:{port}") as socket:
            return await asyncio.wait_for(script(socket), WAIT_S)


async def received(socket) -> list[dict]:
    return [json.loads(message) async for message in socket]


@pytest.mark.parametrize(
    ("init", "error"),
    [
        (HELLO, None),
        ({**HELLO, "audioEncoding": "PCM_16KHZ"}, "Only PCM_24KHZ audio is supported."),
        ({**HELLO, "mode": "CONTINUOUS"}, "Only ENDPOINTING mode is supported."),
        (["not", "an", "object"], "The first message must be a JSON object."),
    ],
)
def test_the_handshake_accepts_only_what_the_server_sends(init, error):
    assert handshake_error(init) == error


def test_a_stretch_starts_once_shows_growing_text_and_ends_with_its_final():
    turns = Turns()
    steps = [Step("", ""), Step(" Hello", ""), Step(" Hello there", ""), Step("", " Hello there."), Step(" Next", "")]
    events = [event for i, step in enumerate(steps) for event in turns.take(step, i * 160)]
    assert [(e["type"], e.get("transcript"), e["turnId"]) for e in events] == [
        ("speechStart", None, "turn-0"),
        ("transcript", "Hello", "turn-0"),
        ("transcript", "Hello there", "turn-0"),
        ("speechEnd", None, "turn-0"),
        ("speechComplete", "Hello there.", "turn-0"),
        ("speechStart", None, "turn-1"),
        ("transcript", "Next", "turn-1"),
    ]
    assert events[0]["audioProcessedMs"] == 160


def test_a_final_with_no_partial_before_it_still_starts_its_stretch():
    events = Turns().take(Step("", "Yes."), 320)
    assert [e["type"] for e in events] == ["speechStart", "speechEnd", "speechComplete"]


def test_resampling_keeps_a_tone_and_does_not_depend_on_how_audio_is_chunked():
    t = np.arange(INPUT_RATE) / INPUT_RATE
    tone = (0.5 * np.sin(2 * np.pi * 440 * t)).astype(np.float32)
    whole = Resampler().push(tone)
    chunked = Resampler()
    parts = np.concatenate([chunked.push(part) for part in np.array_split(tone, [1, 1921, 5000, 12347])])
    assert whole.size == MODEL_RATE
    np.testing.assert_allclose(parts, whole, atol=1e-5)
    middle = whole[1000:-1000]
    assert abs(np.abs(middle).max() - 0.5) < 0.01
    spectrum = np.abs(np.fft.rfft(middle))
    assert abs(np.argmax(spectrum) * MODEL_RATE / middle.size - 440) < 2


def test_audio_becomes_live_text_and_end_stream_returns_the_last_words_as_final():
    model = FakeModel([Step(" Paste", ""), Step(" Paste that", ""), Step("", " Paste that.")])

    async def script(socket):
        await socket.send(json.dumps(HELLO))
        hello = json.loads(await socket.recv())
        await socket.send(ONE_STEP * 2)
        await socket.send(json.dumps({"type": "endStream"}))
        return [hello, *await received(socket)]

    hello, *events = asyncio.run(session(model, script))
    assert "sessionId" in hello
    assert [(e["type"], e.get("transcript")) for e in events] == [
        ("speechStart", None),
        ("transcript", "Paste"),
        ("transcript", "Paste that"),
        ("speechEnd", None),
        ("speechComplete", "Paste that."),
    ]
    assert model.streams[0].calls == [(0, False), (1, False), (2, True)]


def test_a_dropped_connection_still_ends_the_stream_so_its_state_is_freed():
    model = FakeModel([])

    async def script(socket):
        await socket.send(json.dumps(HELLO))
        await socket.recv()
        await socket.send(ONE_STEP)

    asyncio.run(session(model, script))

    async def ended():
        while not (model.streams and model.streams[0].calls[-1:] == [(1, True)]):
            await asyncio.sleep(0.01)

    asyncio.run(asyncio.wait_for(ended(), WAIT_S))


@pytest.mark.parametrize(
    ("first", "message"),
    [
        ("not json", "The session did not start with a JSON handshake."),
        (json.dumps({**HELLO, "audioEncoding": "OPUS"}), "Only PCM_24KHZ audio is supported."),
    ],
)
def test_a_bad_start_is_refused_in_plain_words(first, message):
    model = FakeModel([])

    async def script(socket):
        await socket.send(first)
        return await received(socket)

    assert asyncio.run(session(model, script)) == [{"type": "error", "message": message}]
    assert model.streams == []


def test_half_a_sample_is_refused_in_plain_words():
    async def script(socket):
        await socket.send(json.dumps(HELLO))
        await socket.recv()
        await socket.send(b"\x00")
        return await received(socket)

    assert asyncio.run(session(FakeModel([]), script)) == [
        {"type": "error", "message": "Audio must be whole 16-bit samples."}
    ]
