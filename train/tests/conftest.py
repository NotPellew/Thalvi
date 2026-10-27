from pathlib import Path

import pytest
from tiny_glimmer import save_tiny_glimmer
from tiny_whisper import DIM, save_tiny_whisper
from transformers import MuseGlimmerForConditionalGeneration, WhisperFeatureExtractor
from transformers.models.whisper.modeling_whisper import WhisperEncoder

from thalvi_train.features import load_encoder


@pytest.fixture(scope="session")
def tiny_glimmer_dir(tmp_path_factory: pytest.TempPathFactory) -> Path:
    path = tmp_path_factory.mktemp("tiny-glimmer")
    save_tiny_glimmer(path)
    return path


@pytest.fixture(scope="session")
def reference_model(tiny_glimmer_dir: Path) -> MuseGlimmerForConditionalGeneration:
    return MuseGlimmerForConditionalGeneration.from_pretrained(tiny_glimmer_dir).eval()


@pytest.fixture(scope="session")
def tiny_whisper_dir(tmp_path_factory: pytest.TempPathFactory) -> Path:
    path = tmp_path_factory.mktemp("tiny-whisper")
    save_tiny_whisper(path)
    return path


@pytest.fixture(scope="session")
def tiny_encoder(tiny_whisper_dir: Path) -> tuple[WhisperEncoder, WhisperFeatureExtractor]:
    return load_encoder(str(tiny_whisper_dir), "cpu", DIM)
