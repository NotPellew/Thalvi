import json
from pathlib import Path

import numpy as np
import pytest
import soundfile
import torch
from transformers import WhisperConfig, WhisperFeatureExtractor, WhisperModel

from thalvi_train.features import (
    INFO_NAME,
    FeatureError,
    encode,
    extract,
    frame_count,
    load_encoder,
    read_audio,
)
from thalvi_train.manifest import Sample, write_manifest
from thalvi_train.records import Record, load_frames

DIM = 32
WINDOW = 32_000
RECORD = "<|start|>assistant to=self<|message|>x<|eot|>"


@pytest.fixture(scope="module")
def tiny_whisper_dir(tmp_path_factory: pytest.TempPathFactory) -> Path:
    path = tmp_path_factory.mktemp("tiny-whisper")
    config = WhisperConfig(
        d_model=DIM,
        encoder_layers=2,
        encoder_attention_heads=2,
        encoder_ffn_dim=64,
        decoder_layers=1,
        decoder_attention_heads=2,
        decoder_ffn_dim=64,
        num_mel_bins=16,
        max_source_positions=WINDOW // 320,
        vocab_size=64,
        pad_token_id=1,
        bos_token_id=1,
        eos_token_id=2,
        decoder_start_token_id=3,
    )
    torch.manual_seed(0)
    WhisperModel(config).save_pretrained(path)
    WhisperFeatureExtractor(feature_size=16, chunk_length=WINDOW // 16_000).save_pretrained(path)
    return path


@pytest.fixture(scope="module")
def tiny(tiny_whisper_dir: Path):
    return load_encoder(str(tiny_whisper_dir), "cpu", encoder_dim=DIM)


def tone(samples: int, seed: int = 0) -> np.ndarray:
    return np.random.default_rng(seed).standard_normal(samples).astype(np.float32) * 0.1


def make_manifest(root: Path, clips: dict[str, tuple[np.ndarray, int]]) -> Path:
    (root / "audio").mkdir(parents=True, exist_ok=True)
    samples = []
    for name, (data, rate) in clips.items():
        soundfile.write(root / "audio" / f"{name}.wav", data, rate)
        samples.append(
            Sample(name, f"audio/{name}.wav", "hello", "answer", "train", "af_x", "kokoro")
        )
    manifest = root / "manifest.jsonl"
    write_manifest(manifest, samples)
    return manifest


@pytest.mark.parametrize(
    ("samples", "frames"),
    [(1, 1), (319, 1), (320, 1), (321, 2), (640, 2), (641, 3), (WINDOW - 1, 100), (WINDOW, 100)],
)
def test_keeps_one_frame_per_320_samples_rounded_up(tiny, samples: int, frames: int) -> None:
    encoder, extractor = tiny
    (out,) = encode(encoder, extractor, [tone(samples)])
    assert frame_count(samples) == frames
    assert out.shape == (frames, DIM)
    assert out.dtype == torch.float16


def test_frame_count_is_the_encoder_frames_whose_window_holds_real_mel_frames(tiny) -> None:
    """Encoder frame `j` reads mel frames `2j-1..2j+1`, so it holds audio iff mel `2j` does."""
    _, extractor = tiny
    for samples in range(1, WINDOW + 1, 7):
        mask = extractor([tone(samples)], sampling_rate=16_000, return_attention_mask=True)
        real_mel = int(mask.attention_mask.sum())
        assert real_mel == -(-samples // 160)
        assert sum(2 * j < real_mel for j in range(WINDOW // 320)) == frame_count(samples)


def test_batched_clips_match_the_same_clips_encoded_alone(tiny) -> None:
    encoder, extractor = tiny
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


def test_written_files_follow_the_load_frames_contract(tmp_path: Path, tiny) -> None:
    manifest = make_manifest(tmp_path, {"a": (tone(24_000), 24_000), "b": (tone(8_000, 1), 16_000)})
    features = tmp_path / "features"
    lines: list[dict[str, object]] = []
    written, skipped = extract(
        manifest, features, *tiny, model="tiny", batch_size=1, log=lines.append
    )
    assert (written, skipped) == (2, 0)
    for name, count in (("a", 50), ("b", 25)):
        record = Record(name, "hello", "answer", "train", RECORD)
        frames = load_frames(features, record, DIM)
        assert frames.shape == (count, DIM)
    assert json.loads((features / INFO_NAME).read_text()) == {
        "model": "tiny",
        "encoder_dim": DIM,
        "frames_per_second": 50,
        "dtype": "float16",
    }
    assert [line["done"] for line in lines] == [1, 2]
    assert all(float(line["clips_per_s"]) > 0 for line in lines)
    assert sorted(p.name for p in features.iterdir()) == [
        "a.safetensors",
        "b.safetensors",
        INFO_NAME,
    ]


def test_resume_skips_files_that_load_and_redoes_the_rest(tmp_path: Path, tiny) -> None:
    manifest = make_manifest(tmp_path, {n: (tone(4_000, i), 16_000) for i, n in enumerate("abc")})
    features = tmp_path / "features"
    extract(manifest, features, *tiny, model="tiny")
    kept = (features / "a.safetensors").stat().st_mtime_ns
    (features / "b.safetensors").unlink()
    (features / "c.safetensors").write_bytes(b"truncated")
    assert extract(manifest, features, *tiny, model="tiny") == (2, 1)
    assert (features / "a.safetensors").stat().st_mtime_ns == kept
    assert load_frames(features, Record("c", "t", "answer", "train", RECORD), DIM).shape[0] == 13


def test_limit_takes_the_first_samples(tmp_path: Path, tiny) -> None:
    manifest = make_manifest(tmp_path, {n: (tone(4_000, i), 16_000) for i, n in enumerate("abc")})
    assert extract(manifest, tmp_path / "f", *tiny, model="tiny", limit=2) == (2, 0)
    assert sorted(p.stem for p in (tmp_path / "f").glob("*.safetensors")) == ["a", "b"]


def test_a_directory_made_by_another_model_is_refused(tmp_path: Path, tiny) -> None:
    manifest = make_manifest(tmp_path, {"a": (tone(4_000), 16_000)})
    extract(manifest, tmp_path / "f", *tiny, model="one")
    with pytest.raises(FeatureError, match="holds"):
        extract(manifest, tmp_path / "f", *tiny, model="two")


def test_loads_the_encoder_weights_of_a_full_checkpoint(tiny_whisper_dir: Path, tiny) -> None:
    reference = WhisperModel.from_pretrained(tiny_whisper_dir).encoder.eval()
    encoder, extractor = tiny
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
        load_encoder(str(tiny_whisper_dir), "cpu")
