import io
from pathlib import Path

import numpy as np
import pytest
import soundfile
import torch
from tiny_whisper import DIM, WINDOW, tone
from transformers import WhisperModel

from thalvi_train.features import (
    FeatureError,
    decode_audio,
    encode,
    encode_long,
    frame_count,
    load_encoder,
    read_audio,
)


@pytest.mark.parametrize(
    ("samples", "frames"),
    [(1, 1), (319, 1), (320, 1), (321, 2), (640, 2), (641, 3), (WINDOW - 1, 100), (WINDOW, 100)],
)
def test_keeps_one_frame_per_320_samples_rounded_up(
    tiny_encoder, samples: int, frames: int
) -> None:
    encoder, extractor = tiny_encoder
    (out,) = encode(encoder, extractor, [tone(samples)])
    assert frame_count(samples) == frames
    assert out.shape == (frames, DIM)
    assert out.dtype == torch.float16


def test_frame_count_is_the_encoder_frames_whose_window_holds_real_mel_frames(tiny_encoder) -> None:
    """Encoder frame `j` reads mel frames `2j-1..2j+1`, so it holds audio iff mel `2j` does."""
    _, extractor = tiny_encoder
    for samples in range(1, WINDOW + 1, 7):
        mask = extractor([tone(samples)], sampling_rate=16_000, return_attention_mask=True)
        real_mel = int(mask.attention_mask.sum())
        assert real_mel == -(-samples // 160)
        assert sum(2 * j < real_mel for j in range(WINDOW // 320)) == frame_count(samples)


def test_batched_clips_match_the_same_clips_encoded_alone(tiny_encoder) -> None:
    encoder, extractor = tiny_encoder
    clips = [tone(5_000, 1), tone(20_000, 2), tone(641, 3)]
    together = encode(encoder, extractor, clips)
    for clip, frames in zip(clips, together, strict=True):
        (alone,) = encode(encoder, extractor, [clip])
        torch.testing.assert_close(frames, alone, atol=1e-3, rtol=1e-3)


def test_resampling_24khz_keeps_the_duration_and_stereo_becomes_mono(tmp_path: Path) -> None:
    left = tone(36_000, 1)
    soundfile.write(tmp_path / "a.wav", np.stack([left, left], axis=1), 24_000)
    audio = read_audio(tmp_path / "a.wav", WINDOW * 2)
    assert audio.shape == (24_000,)
    assert audio.dtype == np.float32
    soundfile.write(tmp_path / "b.wav", left, 16_000)
    np.testing.assert_allclose(read_audio(tmp_path / "b.wav", WINDOW * 2), left, atol=1e-4)


def test_rejects_a_clip_longer_than_the_window(tmp_path: Path) -> None:
    soundfile.write(tmp_path / "long.wav", tone(WINDOW + 1), 16_000)
    with pytest.raises(FeatureError, match="longer than the encoder window"):
        read_audio(tmp_path / "long.wav", WINDOW)


def test_loads_the_encoder_weights_of_a_full_checkpoint(
    tiny_whisper_dir: Path, tiny_encoder
) -> None:
    reference = WhisperModel.from_pretrained(tiny_whisper_dir).encoder.eval()
    encoder, extractor = tiny_encoder
    clip = tone(6_000)
    mel = extractor([clip], sampling_rate=16_000, return_tensors="pt").input_features
    with torch.inference_mode():
        expected = reference(mel).last_hidden_state[0, : frame_count(6_000)]
    (actual,) = encode(encoder, extractor, [clip])
    torch.testing.assert_close(actual.float(), expected, atol=1e-2, rtol=1e-2)
    assert not any(p.requires_grad for p in encoder.parameters())
    assert not encoder.training


def test_refuses_an_encoder_whose_width_is_not_the_projectors(tiny_whisper_dir: Path) -> None:
    with pytest.raises(FeatureError, match=f"width {DIM}.*expects 1280"):
        load_encoder(str(tiny_whisper_dir), "cpu", 1280)


@pytest.mark.parametrize("samples", [WINDOW + 1, 2 * WINDOW, 2 * WINDOW + 5_000])
def test_a_long_clip_is_encoded_in_windows_whose_frames_are_concatenated(
    tiny_encoder, samples: int
) -> None:
    encoder, extractor = tiny_encoder
    clip = tone(samples, 4)
    windows = [clip[start : start + WINDOW] for start in range(0, samples, WINDOW)]
    expected = torch.cat([encode(encoder, extractor, [window])[0] for window in windows])
    actual = encode_long(encoder, extractor, clip)
    assert actual.shape == (frame_count(samples), DIM)
    torch.testing.assert_close(actual, expected, atol=1e-3, rtol=1e-3)


def test_a_short_clip_is_encoded_in_one_window(tiny_encoder) -> None:
    encoder, extractor = tiny_encoder
    clip = tone(5_000, 5)
    torch.testing.assert_close(
        encode_long(encoder, extractor, clip), encode(encoder, extractor, [clip])[0]
    )


def test_audio_decodes_from_bytes_and_unreadable_bytes_are_a_feature_error() -> None:
    wav = io.BytesIO()
    soundfile.write(wav, tone(8_000, 6), 8_000, format="WAV", subtype="PCM_16")
    wav.seek(0)
    assert decode_audio(wav).shape == (16_000,)
    with pytest.raises(FeatureError, match="cannot read audio"):
        decode_audio(io.BytesIO(b"not a wav file at all"))
    empty = io.BytesIO()
    soundfile.write(empty, np.zeros(0, dtype=np.float32), 16_000, format="WAV")
    empty.seek(0)
    with pytest.raises(FeatureError, match="no audio"):
        decode_audio(empty)
