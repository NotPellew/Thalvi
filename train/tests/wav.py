"""WAV request payloads for tests."""

import base64
import io

import soundfile
from tiny_whisper import tone


def wav_base64(samples: int = 16_000, rate: int = 16_000, channels: int = 1, seed: int = 0) -> str:
    audio = tone(samples, seed)
    if channels > 1:
        audio = audio.repeat(channels).reshape(samples, channels)
    buffer = io.BytesIO()
    soundfile.write(buffer, audio, rate, format="WAV", subtype="PCM_16")
    return base64.b64encode(buffer.getvalue()).decode()


def audio_part(**kwargs: int) -> dict:
    return {"type": "input_audio", "input_audio": {"data": wav_base64(**kwargs), "format": "wav"}}
