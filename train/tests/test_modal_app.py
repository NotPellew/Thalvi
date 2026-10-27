import tarfile
from pathlib import Path

import modal
import numpy as np
import pytest
import soundfile
from tiny_whisper import write_audio_manifest

from thalvi_train import modal_app
from thalvi_train.manifest import read_manifest


def test_every_step_is_a_function_with_a_local_entrypoint() -> None:
    steps = {
        "glimmer": modal_app.fetch_glimmer,
        "librispeech": modal_app.fetch_librispeech,
        "teacher": modal_app.run_teacher,
        "train": modal_app.run_train,
        "evaluate": modal_app.run_eval,
    }
    for entrypoint, function in steps.items():
        assert isinstance(function, modal.Function)
        assert callable(getattr(modal_app, entrypoint))


def test_image_builds_from_the_project_directory() -> None:
    assert (modal_app.PROJECT_DIR / "pyproject.toml").is_file()
    assert (modal_app.PROJECT_DIR / "uv.lock").is_file()
    assert (modal_app.PACKAGE_PARENT / "thalvi_train" / "modal_app.py").is_file()


def test_a_step_logs_its_start_and_end_even_when_it_fails(capsys: pytest.CaptureFixture) -> None:
    with pytest.raises(RuntimeError), modal_app.logged("teacher", "H100"):
        raise RuntimeError("boom")
    start, end = capsys.readouterr().out.splitlines()
    assert start.startswith("[teacher] start ") and start.endswith(" on H100")
    assert end.startswith("[teacher] end ") and end.endswith(" h on H100")


def fake_download(root: Path, after_subset=lambda: None) -> None:
    folder = root / "LibriSpeech" / "dev-clean" / "84" / "121123"
    folder.mkdir(parents=True)
    soundfile.write(folder / "84-121123-0000.flac", np.zeros(8000), 8000)
    (folder / "84-121123.trans.txt").write_text("84-121123-0000 HELLO THERE\n")


@pytest.fixture
def volume_speech(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    write_audio_manifest(tmp_path / "source" / "speech", {"k-1": 1600})
    tar_path = tmp_path / "speech.tar"
    with tarfile.open(tar_path, "w") as tar:
        tar.add(tmp_path / "source" / "speech", arcname="speech")
    monkeypatch.setattr(modal_app, "SPEECH_TAR", str(tar_path))
    monkeypatch.setattr(modal_app, "STAGE_ROOT", str(tmp_path / "stage"))
    monkeypatch.setattr(modal_app, "LIBRISPEECH_MANIFEST", str(tmp_path / "absent.jsonl"))


def test_staging_puts_both_audio_sets_on_local_disk_and_names_their_manifests(
    volume_speech: None, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(modal_app, "download_librispeech", fake_download)
    args = modal_app.stage_audio()

    assert args[::2] == ["--audio-manifest"] * 2
    speech, librispeech = (Path(path) for path in args[1::2])
    assert speech == tmp_path / "stage" / "speech" / "manifest.jsonl"
    (kokoro,) = read_manifest(speech)
    assert (speech.parent / kokoro.audio).is_file()
    (read,) = read_manifest(librispeech)
    assert read.id == "ls-84-121123-0000"
    assert (librispeech.parent / read.audio).is_file()


def test_the_teacher_reads_the_speech_manifest_from_the_tar_without_its_audio(
    volume_speech: None, tmp_path: Path
) -> None:
    assert modal_app.teacher_manifests() == [str(tmp_path / "stage" / "speech" / "manifest.jsonl")]
    assert not (tmp_path / "stage" / "speech" / "audio").exists()


def test_the_teacher_needs_some_manifest(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(modal_app, "SPEECH_TAR", str(tmp_path / "none.tar"))
    monkeypatch.setattr(modal_app, "LIBRISPEECH_MANIFEST", str(tmp_path / "none.jsonl"))
    with pytest.raises(FileNotFoundError, match=r"none\.tar"):
        modal_app.teacher_manifests()
