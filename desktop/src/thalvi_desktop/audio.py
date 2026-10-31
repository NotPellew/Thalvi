"""Owns native microphone frames and queued speech playback; no audio is saved."""

import asyncio
import queue
import threading
from contextlib import suppress


class Audio:
    def __init__(self, device: str | int | None = None):
        self.device = device
        self.frames: asyncio.Queue[bytes | Exception] = asyncio.Queue(125)
        self.output: queue.Queue[bytes] = queue.Queue(1500)
        self.tail = b""
        self.sent_bytes = 0
        self.played_bytes = 0
        self.closed = False
        self.failed = False
        self.input_stream = None
        self.output_stream = None
        self.output_lock = threading.Lock()
        self.input_lock = threading.Lock()
        self.input_epoch = 0

    async def __aenter__(self) -> "Audio":
        try:
            import sounddevice as sd
        except ImportError as error:
            raise RuntimeError(
                "Install the desktop audio extra before opening the microphone."
            ) from error
        loop = asyncio.get_running_loop()

        def offer(data: bytes, status: str, epoch: int) -> None:
            if epoch != self.input_epoch or self.closed or self.failed:
                return
            if status or self.frames.full():
                self.failed = True
                self.discard()
                self.frames.put_nowait(
                    RuntimeError("Microphone audio was lost. Capture stopped without delivery.")
                )
            else:
                self.frames.put_nowait(data)

        def callback(data, _frames, _time, status):
            with self.input_lock:
                epoch = self.input_epoch
            loop.call_soon_threadsafe(offer, bytes(data), str(status) if status else "", epoch)

        try:
            self.input_stream = sd.RawInputStream(
                samplerate=48_000,
                blocksize=3840,
                channels=1,
                dtype="int16",
                device=self.device,
                callback=callback,
            )
            await asyncio.to_thread(self.input_stream.start)
        except Exception as error:
            if self.input_stream is not None:
                self.input_stream.close()
            raise RuntimeError(
                "The microphone could not open at 48 kHz. Check the device and permission."
            ) from error
        return self

    async def __aexit__(self, *_args) -> None:
        self.closed = True
        if self.input_stream is not None:
            await asyncio.to_thread(self.input_stream.close)
        if self.output_stream is not None:
            await asyncio.to_thread(self.output_stream.close)

    async def next(self) -> bytes:
        if self.failed and self.frames.empty():
            raise RuntimeError("Microphone audio was lost. Restart the desktop client.")
        frame = await self.frames.get()
        if isinstance(frame, Exception):
            raise frame
        if self.output_stream is not None and not self.output_stream.active:
            raise RuntimeError("Speech playback stopped. Check the output device.")
        return frame

    def discard(self) -> None:
        while not self.frames.empty():
            self.frames.get_nowait()

    def reset_input(self) -> None:
        with self.input_lock:
            self.input_epoch += 1
        self.discard()

    def play(self, pcm: bytes) -> None:
        if len(pcm) % 2:
            raise RuntimeError("Speech returned an incomplete audio sample.")
        if self.output_stream is None:
            import sounddevice as sd

            def callback(out, _frames, _time, _status):
                with self.output_lock:
                    need = len(out)
                    while len(self.tail) < need:
                        try:
                            self.tail += self.output.get_nowait()
                        except queue.Empty:
                            break
                    chunk, self.tail = self.tail[:need], self.tail[need:]
                    self.played_bytes += len(chunk)
                    out[:] = chunk + bytes(need - len(chunk))

            try:
                self.output_stream = sd.RawOutputStream(
                    samplerate=24_000, blocksize=480, channels=1, dtype="int16", callback=callback
                )
                self.output_stream.start()
            except Exception as error:
                if self.output_stream is not None:
                    self.output_stream.close()
                self.output_stream = None
                raise RuntimeError(
                    "Speech playback could not open. Check the output device."
                ) from error
        with self.output_lock:
            if self.output.full() or self.sent_bytes - self.played_bytes + len(pcm) > 1_440_000:
                self._stop_locked()
                raise RuntimeError("Speech playback fell too far behind and was stopped.")
            self.output.put_nowait(pcm)
            self.sent_bytes += len(pcm)

    def stop(self) -> None:
        with self.output_lock:
            self._stop_locked()

    def _stop_locked(self) -> None:
        with suppress(queue.Empty):
            while True:
                self.output.get_nowait()
        self.tail = b""
        self.played_bytes = self.sent_bytes

    def new_conversation(self) -> None:
        self.stop()
        with self.output_lock:
            self.played_bytes = self.sent_bytes = 0

    def drained_bytes(self) -> int | None:
        with self.output_lock:
            return self.played_bytes if self.played_bytes == self.sent_bytes else None


def samples(frame: bytes, divisor: int):
    import numpy as np

    values = np.frombuffer(frame, dtype="<i2")
    if values.size % divisor:
        raise RuntimeError("The microphone returned a misaligned audio frame.")
    return values.astype(np.int32).reshape(-1, divisor).mean(axis=1).astype("<i2")


def transcription_pcm(frame: bytes) -> bytes:
    return samples(frame, 2).tobytes()
