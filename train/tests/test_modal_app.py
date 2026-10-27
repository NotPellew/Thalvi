import tarfile
from collections.abc import Sequence
from pathlib import Path

import modal
import numpy as np
import pytest
import soundfile
from corpus_files import write_fleurs, write_musan, write_voxpopuli
from tiny_whisper import write_audio_manifest

from thalvi_train import augment, corpora, modal_app
from thalvi_train.manifest import read_manifest


def test_every_step_is_a_function_with_a_local_entrypoint() -> None:
    steps = {
        "glimmer": modal_app.fetch_glimmer,
        "librispeech": modal_app.fetch_librispeech,
        "corpora": modal_app.fetch_corpora,
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


def fake_download(root: Path, subsets: Sequence[str] = (), *_: object) -> None:
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
    monkeypatch.setattr(modal_app, "ARCHIVES_DIR", str(tmp_path / "archives"))


@pytest.fixture
def volume_corpora(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Packs one clip of each new corpus, as `corpora` would, onto the fake volume."""
    monkeypatch.setattr(modal_app, "ARCHIVES_DIR", str(tmp_path / "archives"))
    vp_rows = [
        {"id": "v-1", "raw": "Hi there.", "normalized": "hi", "speaker": "5", "seconds": 0.5}
    ]
    fl_rows = [{"stem": "99", "sentence": 1, "text": "Wie geht's?", "gender": 0, "seconds": 0.5}]
    corpora.write_clips(
        corpora.voxpopuli_clips(
            write_voxpopuli(tmp_path / "vp.parquet", vp_rows), lambda _: "train"
        ),
        tmp_path / "build" / "voxpopuli",
        "voxpopuli",
    )
    corpora.write_clips(
        corpora.fleurs_clips(write_fleurs(tmp_path / "fl.parquet", fl_rows), "de", "eval"),
        tmp_path / "build" / "fleurs",
        "fleurs",
    )
    musan = write_musan(tmp_path / "build")
    corpora.write_noise_manifest(musan / "manifest.jsonl", corpora.list_musan(musan))
    for name in (*modal_app.SPEECH_CORPORA, modal_app.MUSAN):
        corpora.pack_tar(tmp_path / "build" / name, modal_app.corpus_archive(name))


def test_staging_puts_all_audio_on_local_disk_and_names_the_manifests(
    volume_speech: None, volume_corpora: None, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(modal_app, "download_librispeech", fake_download)
    args = modal_app.stage_audio(modal_app.EVAL_SUBSETS)

    stage = tmp_path / "stage"
    assert args[::2] == ["--audio-manifest"] * 4 + ["--noise-manifest"]
    speech, librispeech, voxpopuli, fleurs, noise = (Path(path) for path in args[1::2])
    assert speech == stage / "speech" / "manifest.jsonl"
    assert voxpopuli == stage / "voxpopuli" / "manifest.jsonl"
    assert fleurs == stage / "fleurs" / "manifest.jsonl"
    assert noise == stage / "musan" / "manifest.jsonl"
    (kokoro,) = read_manifest(speech)
    (read,) = read_manifest(librispeech)
    (real,) = read_manifest(voxpopuli)
    (german,) = read_manifest(fleurs)
    assert (kokoro.source, read.id, real.id, german.id) == (
        "kokoro",
        "ls-84-121123-0000",
        "vp-v-1",
        "fl-de-99",
    )
    for manifest, sample in ((speech, kokoro), (librispeech, read), (voxpopuli, real)):
        assert (manifest.parent / sample.audio).is_file()
    assert (fleurs.parent / german.audio).is_file()
    assert len(augment.read_noise_manifest(noise)) == 4


def test_staging_without_the_corpora_archives_says_to_run_corpora(
    volume_speech: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(modal_app, "download_librispeech", fake_download)
    with pytest.raises(FileNotFoundError, match=r"voxpopuli\.tar is missing.*::corpora"):
        modal_app.stage_audio(modal_app.EVAL_SUBSETS)


def test_the_teacher_reads_every_manifest_from_its_tar_without_the_audio(
    volume_speech: None, volume_corpora: None, tmp_path: Path
) -> None:
    stage = tmp_path / "stage"
    assert modal_app.teacher_manifests() == [
        str(stage / "speech" / "manifest.jsonl"),
        str(stage / "voxpopuli" / "manifest.jsonl"),
        str(stage / "fleurs" / "manifest.jsonl"),
    ]
    assert not (stage / "speech" / "audio").exists()
    assert not (stage / "voxpopuli" / "audio").exists()
    assert not (stage / "musan").exists()


def test_the_teacher_answers_only_the_manifests_that_exist(
    volume_speech: None, tmp_path: Path
) -> None:
    assert modal_app.teacher_manifests() == [str(tmp_path / "stage" / "speech" / "manifest.jsonl")]


def test_the_teacher_needs_some_manifest(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(modal_app, "SPEECH_TAR", str(tmp_path / "none.tar"))
    monkeypatch.setattr(modal_app, "LIBRISPEECH_MANIFEST", str(tmp_path / "none.jsonl"))
    monkeypatch.setattr(modal_app, "ARCHIVES_DIR", str(tmp_path / "archives"))
    with pytest.raises(FileNotFoundError, match=r"none\.tar"):
        modal_app.teacher_manifests()


class Volume:
    def __init__(self) -> None:
        self.commits = 0

    def commit(self) -> None:
        self.commits += 1


@pytest.fixture
def downloads(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> dict[str, list[str]]:
    """Stands in for the network: Hugging Face files become tiny parquet files, and the
    MUSAN archive a tiny tarball; the calls made are recorded by file or archive name.
    """
    monkeypatch.setattr(modal_app, "ARCHIVES_DIR", str(tmp_path / "archives"))
    monkeypatch.setattr(modal_app, "CORPORA_WORK", str(tmp_path / "work"))
    monkeypatch.setattr(modal_app, "data_volume", Volume())
    calls: dict[str, list[str]] = {"parquet": [], "archive": []}
    speakers = iter(range(1000, 2000))

    def row(name: str) -> dict:
        return {
            "id": f"{name}-{next(speakers)}",
            "raw": "Hello.",
            "normalized": "hello",
            "speaker": str(next(speakers)),
            "seconds": 0.5,
        }

    def parquet(repo: str, revision: str, filename: str) -> Path:
        calls["parquet"].append(filename)
        path = tmp_path / "hf" / filename
        path.parent.mkdir(parents=True, exist_ok=True)
        if repo == modal_app.FLEURS_REPO:
            fleurs = {"stem": filename.replace("/", "-"), "sentence": 1, "gender": 1}
            return write_fleurs(path, [{**fleurs, "text": "Hallo.", "seconds": 0.5}])
        return write_voxpopuli(path, [row(filename[:2]) for _ in range(20)])

    def archive(url: str, filename: str) -> Path:
        calls["archive"].append(filename)
        path = tmp_path / "musan.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            tar.add(write_musan(tmp_path / "source"), arcname="musan")
        return path

    monkeypatch.setattr(modal_app, "hf_parquet", parquet)
    monkeypatch.setattr(modal_app, "cached_archive", archive)
    return calls


@pytest.mark.filterwarnings("ignore:The fetch_corpora function is executing locally")
def test_corpora_packs_each_dataset_and_a_rerun_does_nothing_more(
    downloads: dict[str, list[str]], tmp_path: Path
) -> None:
    modal_app.fetch_corpora.local()

    archives = tmp_path / "archives"
    assert sorted(path.name for path in archives.iterdir()) == [
        "fleurs.tar",
        "musan.tar",
        "voxpopuli.tar",
    ]
    assert modal_app.data_volume.commits == 3
    assert not (tmp_path / "work" / "voxpopuli").exists()
    vp = modal_app.extract_manifest(str(archives / "voxpopuli.tar"), "voxpopuli/manifest.jsonl")
    samples = read_manifest(Path(vp))
    assert {sample.split for sample in samples} == {"train", "eval"}
    assert len(downloads["parquet"]) == 3 + 6
    fleurs = read_manifest(
        Path(modal_app.extract_manifest(str(archives / "fleurs.tar"), "fleurs/manifest.jsonl"))
    )
    assert sorted((s.id[:5], s.split) for s in fleurs) == [
        ("fl-de", "eval"),
        ("fl-de", "eval"),
        ("fl-de", "train"),
        ("fl-en", "eval"),
        ("fl-en", "eval"),
        ("fl-en", "train"),
    ]

    modal_app.fetch_corpora.local()
    assert len(downloads["parquet"]) == 9
    assert modal_app.data_volume.commits == 3


@pytest.mark.filterwarnings("ignore:The run_train function is executing locally")
def test_training_passes_extra_arguments_such_as_an_init_run_to_the_trainer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    commands: list[list[str]] = []
    monkeypatch.setattr(modal_app, "stage_audio", lambda subsets: ["--audio-manifest", "m"])
    monkeypatch.setattr(modal_app, "run_logged", commands.append)

    modal_app.run_train.local(
        "projector-v3", "--init-projector /data/runs/projector-v2 --history-prob 0.5"
    )

    (command,) = commands
    tail = command[command.index("--device") :]
    assert tail == [
        "--device",
        "cuda",
        "--init-projector",
        "/data/runs/projector-v2",
        "--history-prob",
        "0.5",
    ]
    assert command[command.index("--out-dir") + 1] == "/data/runs/projector-v3"
