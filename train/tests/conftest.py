from pathlib import Path

import pytest
from tiny_glimmer import save_tiny_glimmer
from transformers import MuseGlimmerForConditionalGeneration


@pytest.fixture(scope="session")
def tiny_glimmer_dir(tmp_path_factory: pytest.TempPathFactory) -> Path:
    path = tmp_path_factory.mktemp("tiny-glimmer")
    save_tiny_glimmer(path)
    return path


@pytest.fixture(scope="session")
def reference_model(tiny_glimmer_dir: Path) -> MuseGlimmerForConditionalGeneration:
    return MuseGlimmerForConditionalGeneration.from_pretrained(tiny_glimmer_dir).eval()
