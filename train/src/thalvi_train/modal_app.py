"""The Modal app that runs the adapter pipeline's download and GPU steps on two volumes.

`thalvi-glimmer` holds Glimmer's weights; `thalvi-data` holds LibriSpeech, the
Kokoro speech uploaded from the local machine, the Whisper features, the teacher's
answers and training runs. Each step is a function plus a local entrypoint of
the shorter name, so `modal run src/thalvi_train/modal_app.py::teacher` runs it.

Two images keep versions apart. Most steps use the project's own `uv.lock` with
the CUDA torch extra, so Modal runs the code and versions the local machine does. The
teacher step uses vLLM, which pins its own torch and transformers (`< 5.20`,
older than the project's), so it gets an image of its own and reads only
`thalvi_train.teacher`, `prompts`, `records` and `manifest`.

Every GPU step prints when it starts and ends and the hours between, which
times the GPU rate for the cost of the step. Image pulls and container start
are not in that figure. Long steps commit the volume while they run, so a
timeout loses minutes of work, not all of it.
"""

import contextlib
import datetime
import os
import shlex
import subprocess
import sys
import tarfile
import time
from collections.abc import Iterator, Sequence
from pathlib import Path

import modal

GPU = "H100"
FEATURES_GPUS = ["L4", "A10G", "L40S"]

GLIMMER_REPO = "meta-models/Muse-Glimmer-30B"
GLIMMER_REVISION = "cec94fcfd6512f2cfe708138c32533b346ea4f69"
VLLM_VERSION = "0.32.0"
LIBRISPEECH_URL = "https://openslr.trmal.net/resources/12/{subset}.tar.gz"
LIBRISPEECH_SUBSETS = ("train-clean-100", "dev-clean", "test-clean")

GLIMMER_MOUNT = "/glimmer"
DATA_MOUNT = "/data"
GLIMMER_DIR = f"{GLIMMER_MOUNT}/Muse-Glimmer-30B"
GLIMMER_TEXT_DIR = f"{GLIMMER_MOUNT}/Muse-Glimmer-30B-text"
TEXT_SHARD_BYTES = 4 * 1024**3
SPEECH_MANIFEST = f"{DATA_MOUNT}/speech/manifest.jsonl"
LIBRISPEECH_ROOT = f"{DATA_MOUNT}/librispeech"
LIBRISPEECH_MANIFEST = f"{LIBRISPEECH_ROOT}/manifest.jsonl"
FEATURES_DIR = f"{DATA_MOUNT}/features"
TEACHER_OUT = f"{DATA_MOUNT}/teacher/train.jsonl"
RUNS_DIR = f"{DATA_MOUNT}/runs"

VLLM_PORT = 8000
SERVED_NAME = "glimmer"
VLLM_START_TIMEOUT = 30 * 60
COMMIT_EVERY = 5 * 60

HOUR = 60 * 60
PACKAGE_PARENT = Path(__file__).resolve().parent.parent
PROJECT_DIR = PACKAGE_PARENT.parent

app = modal.App("thalvi-adapter")
glimmer_volume = modal.Volume.from_name("thalvi-glimmer", create_if_missing=True)
data_volume = modal.Volume.from_name("thalvi-data", create_if_missing=True)

project_image = (
    modal.Image.debian_slim(python_version="3.12")
    .uv_sync(str(PROJECT_DIR), extras=["cu128"], extra_options="--no-dev")
    .env({"PYTHONUNBUFFERED": "1"})
    .add_local_python_source("thalvi_train")
)
vllm_image = (
    modal.Image.debian_slim(python_version="3.12")
    .uv_pip_install(f"vllm=={VLLM_VERSION}")
    .env(
        {
            "PYTHONUNBUFFERED": "1",
            "HF_HUB_OFFLINE": "1",
            "VLLM_NO_USAGE_STATS": "1",
            "VLLM_USE_FLASHINFER_SAMPLER": "0",
        }
    )
    .add_local_python_source("thalvi_train")
)


@contextlib.contextmanager
def logged(step: str, gpu: str | None) -> Iterator[None]:
    """Prints the step's start and end times and the hours between, for pricing it."""
    started = time.monotonic()
    print(f"[{step}] start {_utc_now()} on {gpu or 'cpu'}", flush=True)
    try:
        yield
    finally:
        hours = (time.monotonic() - started) / HOUR
        print(f"[{step}] end {_utc_now()}; {hours:.2f} h on {gpu or 'cpu'}", flush=True)


def _utc_now() -> str:
    return datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%d %H:%M:%S UTC")


def run_logged(command: Sequence[str]) -> None:
    """Runs `command` with its output in the Modal log, committing `thalvi-data` as it goes."""
    environment = {**os.environ, "PYTHONPATH": str(PACKAGE_PARENT)}
    process = subprocess.Popen(command, env=environment)
    try:
        while True:
            try:
                code = process.wait(timeout=COMMIT_EVERY)
                break
            except subprocess.TimeoutExpired:
                data_volume.commit()
    finally:
        if process.poll() is None:
            process.kill()
        data_volume.commit()
    if code != 0:
        raise subprocess.CalledProcessError(code, list(command))


@app.function(image=project_image, volumes={GLIMMER_MOUNT: glimmer_volume}, timeout=3 * HOUR)
def fetch_glimmer() -> None:
    """Downloads Glimmer onto `thalvi-glimmer`, plus the text-only resharded copy training loads.

    vLLM serves the original; training loads the copy (see `thalvi_train.reshard`).
    """
    from huggingface_hub import snapshot_download

    from thalvi_train.reshard import reshard

    with logged("glimmer", None):
        path = Path(
            snapshot_download(GLIMMER_REPO, revision=GLIMMER_REVISION, local_dir=GLIMMER_DIR)
        )
        shards = sorted(path.glob("*.safetensors"))
        gigabytes = sum(shard.stat().st_size for shard in shards) / 1e9
        print(f"{len(shards)} shards, {gigabytes:.1f} GB in {path}", flush=True)
        reshard(path, Path(GLIMMER_TEXT_DIR), TEXT_SHARD_BYTES)
        glimmer_volume.commit()


@app.function(image=project_image, volumes={DATA_MOUNT: data_volume}, timeout=3 * HOUR)
def fetch_librispeech() -> None:
    """Downloads and extracts the LibriSpeech subsets, then writes their manifest."""
    import httpx

    from thalvi_train import librispeech

    root = Path(LIBRISPEECH_ROOT)
    with logged("librispeech", None):
        for subset in LIBRISPEECH_SUBSETS:
            final = root / "LibriSpeech" / subset
            if final.is_dir():
                print(f"{subset}: already extracted", flush=True)
                continue
            archive = Path("/tmp") / f"{subset}.tar.gz"
            with (
                httpx.stream(
                    "GET", LIBRISPEECH_URL.format(subset=subset), follow_redirects=True
                ) as response,
                archive.open("wb") as file,
            ):
                response.raise_for_status()
                for chunk in response.iter_bytes(1 << 20):
                    file.write(chunk)
            staging = root / f".staging-{subset}"
            with tarfile.open(archive) as tar:
                tar.extractall(staging, filter="data")
            final.parent.mkdir(parents=True, exist_ok=True)
            (staging / "LibriSpeech" / subset).replace(final)
            archive.unlink()
            data_volume.commit()
            print(f"{subset}: extracted", flush=True)
        librispeech.main([str(root), LIBRISPEECH_MANIFEST])
        data_volume.commit()


@contextlib.contextmanager
def vllm_server() -> Iterator[str]:
    """Serves Glimmer's text model with vLLM and yields its base URL once it is healthy."""
    import httpx

    command = [
        "vllm",
        "serve",
        GLIMMER_DIR,
        "--served-model-name",
        SERVED_NAME,
        "--dtype",
        "bfloat16",
        "--language-model-only",
        "--max-model-len",
        "4096",
        "--port",
        str(VLLM_PORT),
    ]
    process = subprocess.Popen(command)
    base_url = f"http://127.0.0.1:{VLLM_PORT}"
    try:
        deadline = time.monotonic() + VLLM_START_TIMEOUT
        while True:
            if process.poll() is not None:
                raise RuntimeError(f"vllm serve exited with {process.returncode} while starting")
            try:
                if httpx.get(f"{base_url}/health").status_code == 200:
                    break
            except httpx.TransportError:
                pass
            if time.monotonic() > deadline:
                raise TimeoutError(f"vllm serve not healthy after {VLLM_START_TIMEOUT} s")
            time.sleep(5)
        print("vllm is serving", flush=True)
        yield base_url
    finally:
        process.terminate()
        try:
            process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            process.kill()


@app.function(
    image=vllm_image,
    gpu=GPU,
    volumes={GLIMMER_MOUNT: glimmer_volume, DATA_MOUNT: data_volume},
    timeout=6 * HOUR,
)
def run_teacher(limit: int, temperature: float, max_tokens: int, concurrency: int) -> None:
    """Has Glimmer read each transcript and answer, appending to the training manifest."""
    manifests = [m for m in (SPEECH_MANIFEST, LIBRISPEECH_MANIFEST) if Path(m).exists()]
    if not manifests:
        raise FileNotFoundError(f"no manifest at {SPEECH_MANIFEST} or {LIBRISPEECH_MANIFEST}")
    with logged("teacher", GPU), vllm_server() as base_url:
        run_logged(
            [
                sys.executable,
                "-m",
                "thalvi_train.teacher",
                *manifests,
                "--out",
                TEACHER_OUT,
                "--glimmer",
                GLIMMER_DIR,
                "--model",
                SERVED_NAME,
                "--base-url",
                base_url,
                "--temperature",
                str(temperature),
                "--max-tokens",
                str(max_tokens),
                "--concurrency",
                str(concurrency),
                *(["--limit", str(limit)] if limit else []),
            ]
        )


@app.function(
    image=project_image, gpu=FEATURES_GPUS, volumes={DATA_MOUNT: data_volume}, timeout=3 * HOUR
)
def run_features(batch_size: int) -> None:
    """Caches Whisper encoder features for every data manifest into one directory."""
    manifests = [m for m in (SPEECH_MANIFEST, LIBRISPEECH_MANIFEST) if Path(m).exists()]
    with logged("features", "/".join(FEATURES_GPUS)):
        for manifest in manifests:
            run_logged(
                [
                    sys.executable,
                    "-m",
                    "thalvi_train.features",
                    manifest,
                    FEATURES_DIR,
                    "--device",
                    "cuda",
                    "--batch-size",
                    str(batch_size),
                ]
            )


@app.function(
    image=project_image,
    gpu=GPU,
    volumes={GLIMMER_MOUNT: glimmer_volume, DATA_MOUNT: data_volume},
    timeout=12 * HOUR,
)
def run_train(run_name: str, extra_args: str) -> None:
    """Trains the projector on unquantized bf16 Glimmer; `extra_args` go to `thalvi_train.train`."""
    with logged("train", GPU):
        run_logged(
            [
                sys.executable,
                "-m",
                "thalvi_train.train",
                "--manifest",
                TEACHER_OUT,
                "--features-dir",
                FEATURES_DIR,
                "--glimmer",
                GLIMMER_TEXT_DIR,
                "--out-dir",
                f"{RUNS_DIR}/{run_name}",
                "--device",
                "cuda",
                *shlex.split(extra_args),
            ]
        )


@app.function(
    image=project_image,
    gpu=GPU,
    volumes={GLIMMER_MOUNT: glimmer_volume, DATA_MOUNT: data_volume},
    timeout=6 * HOUR,
)
def run_eval(run_name: str, limit: int, extra_args: str) -> None:
    """Evaluates `runs/<run_name>` on unquantized bf16 Glimmer, writing `eval.jsonl` beside it.

    The summary lands next to the rows as `eval.summary.json`; `extra_args` go to
    `thalvi_train.evaluate`.
    """
    run_dir = f"{RUNS_DIR}/{run_name}"
    with logged("evaluate", GPU):
        run_logged(
            [
                sys.executable,
                "-m",
                "thalvi_train.evaluate",
                "--manifest",
                TEACHER_OUT,
                "--features-dir",
                FEATURES_DIR,
                "--glimmer",
                GLIMMER_TEXT_DIR,
                "--projector",
                run_dir,
                "--out",
                f"{run_dir}/eval.jsonl",
                "--device",
                "cuda",
                "--limit",
                str(limit),
                *shlex.split(extra_args),
            ]
        )


@app.local_entrypoint()
def glimmer() -> None:
    """Step 1: download Glimmer onto `thalvi-glimmer`."""
    fetch_glimmer.remote()


@app.local_entrypoint()
def librispeech() -> None:
    """Step 2: download LibriSpeech onto `thalvi-data` and write its manifest."""
    fetch_librispeech.remote()


@app.local_entrypoint()
def teacher(
    limit: int = 0, temperature: float = 0.0, max_tokens: int = 512, concurrency: int = 64
) -> None:
    """Step 3: Glimmer answers every transcript; `--limit N` answers only the first N."""
    run_teacher.remote(limit, temperature, max_tokens, concurrency)


@app.local_entrypoint()
def features(batch_size: int = 32) -> None:
    """Step 4: cache Whisper encoder features for every sample."""
    run_features.remote(batch_size)


@app.local_entrypoint()
def train(run_name: str = "projector", extra_args: str = "") -> None:
    """Step 5: train the projector into `runs/<run_name>`; `--extra-args` goes to the trainer."""
    run_train.remote(run_name, extra_args)


@app.local_entrypoint()
def evaluate(run_name: str = "projector", limit: int = 500, extra_args: str = "") -> None:
    """Step 6: score `runs/<run_name>` on the first `--limit` eval samples; extras go to the CLI."""
    run_eval.remote(run_name, limit, extra_args)
