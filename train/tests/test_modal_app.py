import modal
import pytest

from thalvi_train import modal_app


def test_every_step_is_a_function_with_a_local_entrypoint() -> None:
    steps = {
        "glimmer": modal_app.fetch_glimmer,
        "librispeech": modal_app.fetch_librispeech,
        "teacher": modal_app.run_teacher,
        "features": modal_app.run_features,
        "train": modal_app.run_train,
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
