"""The Modal app that runs the adapter pipeline's download and GPU steps on two volumes.

`thalvi-glimmer` holds Glimmer's weights; `thalvi-data` holds LibriSpeech, the
Kokoro speech uploaded from the local machine as `speech.tar`, the real-voice
corpora and MUSAN noise that `corpora` packs into `archives/`, the teacher's
answers and training runs. Each step is a function plus a local entrypoint of
the shorter name, so `modal run src/thalvi_train/modal_app.py::teacher` runs it.

Two images keep versions apart. Most steps use the project's own `uv.lock` with
the CUDA torch extra, so Modal runs the code and versions the local machine does. The
teacher step uses vLLM, which pins its own torch and transformers (`< 5.20`,
older than the project's), so it gets an image of its own and reads only
`thalvi_train.teacher`, `prompts`, `records` and `manifest`.

Training and evaluation run the Whisper encoder inline. A volume serves small files
at ~0.2 s each, so every corpus lives on the volume as one big archive; they first
stage the archives onto the container's local disk (`stage_audio`) and read the
audio from there; the staging time is in the log.

`run_cascade` scores the cascaded baseline, Muse Voice Transcribe, on the same eval clips on
CPU, with the Meta API key from the `thalvi-meta-asr` secret.

`Endpoint` serves the chat endpoint of `thalvi_train.serve` on an H100: unquantized bf16
Glimmer from the volume, the Whisper encoder in fp16, and the projector of the run named by
`THALVI_PROJECTOR_RUN` (default `projector-v2`). Its `thalvi-endpoint` secret holds the bearer
key. `modal deploy src/thalvi_train/modal_app.py` publishes it, `modal app stop thalvi-adapter`
takes it down; it scales to zero five minutes after the last request. Decoding one token
launches hundreds of small kernels from Python, so its speed is also the CPU's, hence
cores reserved rather than Modal's default fractional, burstable share.

Every GPU step prints when it starts and ends and the hours between, which
times the GPU rate for the cost of the step. Image pulls and container start
are not in that figure. Long steps commit the volume while they run, so a
timeout loses minutes of work, not all of it.
"""

import contextlib
import datetime
import os
import shlex
import shutil
import subprocess
import sys
import tarfile
import time
from collections.abc import Callable, Iterator, Sequence
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import modal

GPU = "H100"
TRAIN_MEMORY_MIB = 64 * 1024
CORPORA_MEMORY_MIB = 16 * 1024
TRAIN_DISK_MIB = 512 * 1024

GLIMMER_REPO = "meta-models/Muse-Glimmer-30B"
GLIMMER_REVISION = "cec94fcfd6512f2cfe708138c32533b346ea4f69"
VLLM_VERSION = "0.32.0"
LIBRISPEECH_URL = "https://openslr.trmal.net/resources/12/{subset}.tar.gz"
LIBRISPEECH_SUBSETS = ("train-clean-100", "dev-clean", "test-clean")
EVAL_SUBSETS = ("dev-clean", "test-clean")
MUSAN_URL = "https://openslr.trmal.net/resources/17/musan.tar.gz"
VOXPOPULI_REPO = "facebook/voxpopuli"
VOXPOPULI_REVISION = "42f01879c780b4a2e90ec0b4f616c2ece526e4f1"
VOXPOPULI_TRAIN = "en/train-00000-of-00030.parquet"
VOXPOPULI_ACCENTED = "en_accented/test-00000-of-00002.parquet"
VOXPOPULI_TEST = "en/test-00000-of-00001.parquet"
FLEURS_REPO = "google/fleurs"
FLEURS_REVISION = "70bb2e84b976b7e960aa89f1c648e09c59f894dd"
FLEURS_LANGUAGES = {"en": "en_us", "de": "de_de"}
FLEURS_SPLITS = {"train": "train", "validation": "eval", "test": "eval"}

GLIMMER_MOUNT = "/glimmer"
DATA_MOUNT = "/data"
GLIMMER_DIR = f"{GLIMMER_MOUNT}/Muse-Glimmer-30B"
GLIMMER_TEXT_DIR = f"{GLIMMER_MOUNT}/Muse-Glimmer-30B-text"
TEXT_SHARD_BYTES = 4 * 1024**3
SPEECH_TAR = f"{DATA_MOUNT}/speech.tar"
SPEECH_MANIFEST_MEMBER = "speech/manifest.jsonl"
STAGE_ROOT = "/tmp/stage"
ARCHIVES_DIR = f"{DATA_MOUNT}/archives"
SPEECH_CORPORA = ("voxpopuli", "fleurs")
MUSAN = "musan"
CORPORA_WORK = "/tmp/corpora"
LIBRISPEECH_ROOT = f"{DATA_MOUNT}/librispeech"
LIBRISPEECH_MANIFEST = f"{LIBRISPEECH_ROOT}/manifest.jsonl"
TEACHER_OUT = f"{DATA_MOUNT}/teacher/train.jsonl"
RUNS_DIR = f"{DATA_MOUNT}/runs"
WHISPER_MODEL = "openai/whisper-large-v3-turbo"
ENDPOINT_SECRET = "thalvi-endpoint"
ASR_SECRET = "thalvi-meta-asr"
PROJECTOR_RUN_ENV = "THALVI_PROJECTOR_RUN"
DEFAULT_PROJECTOR_RUN = "projector-v2"
ENDPOINT_IDLE_SECONDS = 5 * 60
ENDPOINT_CPUS = 8.0

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
corpora_image = (
    modal.Image.debian_slim(python_version="3.12")
    .uv_sync(str(PROJECT_DIR), extra_options="--no-dev")
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


cascade_image = (
    modal.Image.debian_slim(python_version="3.12")
    .uv_sync(str(PROJECT_DIR), extras=["cpu", "cascade"], extra_options="--no-dev")
    .env({"PYTHONUNBUFFERED": "1"})
    .add_local_python_source("thalvi_train")
)
endpoint_image = (
    modal.Image.debian_slim(python_version="3.12")
    .uv_sync(str(PROJECT_DIR), extras=["cu128", "serve"], extra_options="--no-dev")
    .env({"PYTHONUNBUFFERED": "1"})
    .run_commands(
        'python -c "from huggingface_hub import snapshot_download; '
        f"snapshot_download('{WHISPER_MODEL}')\""
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


def cached_archive(url: str, filename: str) -> Path:
    """`filename` in the archives directory of `thalvi-data`, downloaded from `url` on first use.

    The openslr mirror sends ~4 MB/s to Modal, which made staging train-clean-100
    cost half an hour of H100 time; a big file on the volume reads fast.
    """
    import httpx

    path = Path(ARCHIVES_DIR) / filename
    if path.is_file():
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_name(path.name + ".part")
    with (
        httpx.stream("GET", url, follow_redirects=True, timeout=60.0) as response,
        partial.open("wb") as file,
    ):
        response.raise_for_status()
        for chunk in response.iter_bytes(1 << 20):
            file.write(chunk)
    partial.replace(path)
    data_volume.commit()
    return path


def librispeech_archive(subset: str) -> Path:
    """The subset's archive on `thalvi-data`, downloaded on first use."""
    return cached_archive(LIBRISPEECH_URL.format(subset=subset), f"{subset}.tar.gz")


def download_librispeech(
    root: Path,
    subsets: Sequence[str] = LIBRISPEECH_SUBSETS,
    after_subset: Callable[[], None] = lambda: None,
) -> None:
    """Extracts the LibriSpeech `subsets` missing under `root/LibriSpeech`.

    `after_subset` runs after every extraction.
    """
    for subset in subsets:
        final = root / "LibriSpeech" / subset
        if final.is_dir():
            print(f"{subset}: already extracted", flush=True)
            continue
        staging = root / f".staging-{subset}"
        with tarfile.open(librispeech_archive(subset)) as tar:
            tar.extractall(staging, filter="data")
        final.parent.mkdir(parents=True, exist_ok=True)
        (staging / "LibriSpeech" / subset).replace(final)
        after_subset()
        print(f"{subset}: extracted", flush=True)


def corpus_archive(name: str) -> Path:
    """The packed corpus `name` on `thalvi-data`; `corpora` makes it."""
    return Path(ARCHIVES_DIR) / f"{name}.tar"


def extract_corpus(name: str, root: Path) -> Path:
    """Extracts the corpus archive `name` under `root` and returns its manifest."""
    archive = corpus_archive(name)
    if not archive.is_file():
        raise FileNotFoundError(f"{archive} is missing; run modal_app.py::corpora first")
    with tarfile.open(archive) as tar:
        tar.extractall(root, filter="data")
    return root / name / "manifest.jsonl"


def stage_audio(subsets: Sequence[str]) -> list[str]:
    """Stages all audio on local disk and returns the manifest arguments naming it.

    The LibriSpeech `subsets` come from their archives on the volume and get their own
    manifest; the Kokoro speech comes out of `speech.tar` on `thalvi-data`, with its
    manifest inside, and the VoxPopuli and FLEURS clips out of their archives. The ids
    match the manifests the teacher read on the volume.

    The result is one `--audio-manifest <path>` per speech corpus (Kokoro, LibriSpeech,
    VoxPopuli, FLEURS), then `--noise-manifest <path>` for MUSAN, whose rows are
    `thalvi_train.corpora.NoiseClip`s with paths relative to the manifest's directory.
    """
    from thalvi_train import librispeech

    started = time.monotonic()
    root = Path(STAGE_ROOT)
    root.mkdir(parents=True, exist_ok=True)
    librispeech_root = root / "librispeech"
    download_librispeech(librispeech_root, subsets)
    librispeech_manifest = librispeech_root / "manifest.jsonl"
    librispeech.main([str(librispeech_root), str(librispeech_manifest)])
    with tarfile.open(SPEECH_TAR) as tar:
        tar.extractall(root, filter="data")
    speech_manifest = root / SPEECH_MANIFEST_MEMBER
    if not speech_manifest.is_file():
        raise FileNotFoundError(f"{SPEECH_TAR} holds no {SPEECH_MANIFEST_MEMBER}")
    with ThreadPoolExecutor() as pool:
        *real_manifests, noise_manifest = pool.map(
            lambda name: extract_corpus(name, root), (*SPEECH_CORPORA, MUSAN)
        )
    print(f"staged audio in {time.monotonic() - started:.0f} s", flush=True)
    return [
        *(
            arg
            for manifest in (speech_manifest, librispeech_manifest, *real_manifests)
            for arg in ("--audio-manifest", str(manifest))
        ),
        "--noise-manifest",
        str(noise_manifest),
    ]


def extract_manifest(archive: str, member: str) -> str:
    """Extracts only `member` of `archive` into the staging root and returns its path.

    Stops at the member, which `corpora` writes first, instead of reading the whole archive.
    """
    with tarfile.open(archive) as tar:
        for info in tar:
            if info.name == member:
                tar.extract(info, STAGE_ROOT, filter="data")
                return f"{STAGE_ROOT}/{member}"
    raise FileNotFoundError(f"{archive} holds no {member}")


def teacher_manifests() -> list[str]:
    """The data manifests the teacher answers, whichever exist: the Kokoro speech's and
    the VoxPopuli and FLEURS ones, each read out of its tar without the audio, and
    LibriSpeech's on the volume.
    """
    manifests = []
    if Path(SPEECH_TAR).exists():
        manifests.append(extract_manifest(SPEECH_TAR, SPEECH_MANIFEST_MEMBER))
    if Path(LIBRISPEECH_MANIFEST).exists():
        manifests.append(LIBRISPEECH_MANIFEST)
    manifests.extend(
        extract_manifest(str(corpus_archive(name)), f"{name}/manifest.jsonl")
        for name in SPEECH_CORPORA
        if corpus_archive(name).exists()
    )
    if not manifests:
        raise FileNotFoundError(f"no {SPEECH_TAR} or {LIBRISPEECH_MANIFEST}")
    return manifests


@app.function(image=project_image, volumes={DATA_MOUNT: data_volume}, timeout=3 * HOUR)
def fetch_librispeech() -> None:
    """Caches the LibriSpeech archives on the volume, extracts them, and writes their manifest."""
    from thalvi_train import librispeech

    with logged("librispeech", None):
        for subset in LIBRISPEECH_SUBSETS:
            librispeech_archive(subset)
        download_librispeech(Path(LIBRISPEECH_ROOT), after_subset=data_volume.commit)
        librispeech.main([LIBRISPEECH_ROOT, LIBRISPEECH_MANIFEST])
        data_volume.commit()


def hf_parquet(repo: str, revision: str, filename: str) -> Path:
    """`filename` of the public dataset `repo` at `revision`, downloaded to local disk."""
    from huggingface_hub import hf_hub_download

    return Path(
        hf_hub_download(
            repo,
            filename,
            repo_type="dataset",
            revision=revision,
            local_dir=f"{CORPORA_WORK}/hf/{repo}",
        )
    )


def build_voxpopuli(workdir: Path) -> Path:
    """Converts the VoxPopuli files to `workdir/voxpopuli`: English training and test, and
    accented English with a share held out for eval; no eval speaker has training clips.
    """
    from itertools import chain

    from thalvi_train import corpora

    def parquet(filename: str) -> Path:
        return hf_parquet(VOXPOPULI_REPO, VOXPOPULI_REVISION, filename)

    train, accented, test = (
        parquet(name) for name in (VOXPOPULI_TRAIN, VOXPOPULI_ACCENTED, VOXPOPULI_TEST)
    )
    plan = corpora.plan_voxpopuli(
        *(corpora.speaker_counts(path) for path in (train, accented, test))
    )
    print(
        f"voxpopuli: {len(plan.accented_eval)} accented and {len(plan.test_speakers)} test "
        f"speakers held out; {plan.test_clips_left_out} test clips left out for shared or "
        "unknown speakers",
        flush=True,
    )
    clips = chain(
        corpora.voxpopuli_clips(train, lambda _: "train"),
        corpora.voxpopuli_clips(accented, lambda s: "eval" if s in plan.accented_eval else "train"),
        corpora.voxpopuli_clips(test, lambda _: "eval", plan.test_speakers),
    )
    root = workdir / "voxpopuli"
    written = corpora.write_clips(clips, root, "voxpopuli")
    print(*corpora.summary_lines("voxpopuli", written), sep="\n", flush=True)
    print(f"voxpopuli: {len(corpora.shared_speakers(written))} speakers in both splits", flush=True)
    return root


def build_fleurs(workdir: Path) -> Path:
    """Converts FLEURS English and German to `workdir/fleurs`: train to train, the rest to eval."""
    from itertools import chain

    from thalvi_train import corpora

    clips = chain.from_iterable(
        corpora.fleurs_clips(
            hf_parquet(
                FLEURS_REPO, FLEURS_REVISION, f"parquet-data/{folder}/{name}-00000-of-00001.parquet"
            ),
            language,
            split,
        )
        for language, folder in FLEURS_LANGUAGES.items()
        for name, split in FLEURS_SPLITS.items()
    )
    root = workdir / "fleurs"
    written = corpora.write_clips(clips, root, "fleurs")
    print(
        *corpora.summary_lines(
            "fleurs", written, lambda sample: f"{sample.id.split('-')[1]} {sample.split}"
        ),
        sep="\n",
        flush=True,
    )
    return root


def build_musan(workdir: Path) -> Path:
    """Extracts MUSAN to `workdir/musan` and writes its `NoiseClip` manifest there."""
    from thalvi_train import corpora

    with tarfile.open(cached_archive(MUSAN_URL, "musan.tar.gz")) as tar:
        tar.extractall(workdir, filter="data")
    root = workdir / MUSAN
    clips = corpora.list_musan(root)
    corpora.write_noise_manifest(root / corpora.MANIFEST_NAME, clips)
    for category in ("noise", "music", "speech"):
        chosen = [clip for clip in clips if clip.category == category]
        hours = sum(clip.seconds for clip in chosen) / 3600
        print(f"musan {category}: {len(chosen)} files, {hours:.2f} h", flush=True)
    return root


def pack_corpus(name: str, build: Callable[[Path], Path]) -> None:
    """Builds the corpus `name` on local disk with `build` and packs it into its archive on
    `thalvi-data`, then commits the volume; an archive already there is left alone.
    """
    from thalvi_train import corpora

    archive = corpus_archive(name)
    if archive.is_file():
        print(f"{name}: already packed, {archive.stat().st_size / 1e9:.2f} GB", flush=True)
        return
    with logged(name, None):
        workdir = Path(CORPORA_WORK) / name
        shutil.rmtree(workdir, ignore_errors=True)
        workdir.mkdir(parents=True)
        corpora.pack_tar(build(workdir), archive)
        data_volume.commit()
        shutil.rmtree(workdir)
    print(f"{name}: packed {archive.stat().st_size / 1e9:.2f} GB into {archive}", flush=True)


@app.function(
    image=corpora_image,
    volumes={DATA_MOUNT: data_volume},
    cpu=4,
    memory=CORPORA_MEMORY_MIB,
    ephemeral_disk=TRAIN_DISK_MIB,
    timeout=6 * HOUR,
)
def fetch_corpora() -> None:
    """Downloads VoxPopuli, FLEURS and MUSAN and packs each into a tar on `thalvi-data`.

    A corpus whose archive exists is skipped, so a rerun finishes only what is missing.
    MUSAN's slow download runs in the background while the parquet files convert, and a
    failure in the conversion is raised at once instead of after the download.
    """
    pool = ThreadPoolExecutor(1)
    try:
        with logged("corpora", None):
            download = pool.submit(cached_archive, MUSAN_URL, "musan.tar.gz")
            pack_corpus("voxpopuli", build_voxpopuli)
            pack_corpus("fleurs", build_fleurs)
            download.result()
            pack_corpus(MUSAN, build_musan)
    finally:
        pool.shutdown(wait=False, cancel_futures=True)


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
    manifests = teacher_manifests()
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
    image=project_image,
    gpu=GPU,
    volumes={GLIMMER_MOUNT: glimmer_volume, DATA_MOUNT: data_volume},
    memory=TRAIN_MEMORY_MIB,
    ephemeral_disk=TRAIN_DISK_MIB,
    timeout=12 * HOUR,
)
def run_train(run_name: str, extra_args: str) -> None:
    """Trains the projector on unquantized bf16 Glimmer; `extra_args` go to `thalvi_train.train`.

    `--init-projector /data/runs/<earlier run>` continues from that run's projector.

    The audio is staged onto local disk first, inside the timed step.
    """
    with logged("train", GPU):
        audio_args = stage_audio(LIBRISPEECH_SUBSETS)
        run_logged(
            [
                sys.executable,
                "-m",
                "thalvi_train.train",
                "--manifest",
                TEACHER_OUT,
                *audio_args,
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
    memory=TRAIN_MEMORY_MIB,
    ephemeral_disk=TRAIN_DISK_MIB,
    timeout=6 * HOUR,
)
def run_eval(run_name: str, limit: int, batch_size: int, extra_args: str) -> None:
    """Evaluates `runs/<run_name>` on unquantized bf16 Glimmer, writing `eval.jsonl` beside it.

    The summary lands next to the rows as `eval.summary.json`; `extra_args` go to
    `thalvi_train.evaluate`. The audio is staged onto local disk first, inside the timed step.

    On an 80 GB card the ~52 GB of weights, the ~1.3 GB encoder and the projector leave
    room for 32 samples' KV cache (~53 KB per token, ~1.5 GB) and activations.
    """
    run_dir = f"{RUNS_DIR}/{run_name}"
    with logged("evaluate", GPU):
        audio_args = stage_audio(EVAL_SUBSETS)
        run_logged(
            [
                sys.executable,
                "-m",
                "thalvi_train.evaluate",
                "--manifest",
                TEACHER_OUT,
                *audio_args,
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
                "--batch-size",
                str(batch_size),
                *shlex.split(extra_args),
            ]
        )


@app.function(
    image=cascade_image,
    volumes={DATA_MOUNT: data_volume},
    secrets=[modal.Secret.from_name(ASR_SECRET)],
    cpu=4,
    memory=CORPORA_MEMORY_MIB,
    ephemeral_disk=TRAIN_DISK_MIB,
    timeout=2 * HOUR,
)
def run_cascade(limit: int, extra_args: str) -> None:
    """Scores Muse Voice Transcribe on the first `limit` eval clips; `extra_args` go to
    `thalvi_train.cascade`, which needs at least `--manifest` and `--out`.
    """
    with logged("cascade", None):
        audio_args = stage_audio(EVAL_SUBSETS)
        run_logged(
            [
                sys.executable,
                "-m",
                "thalvi_train.cascade",
                *audio_args,
                "--limit",
                str(limit),
                *shlex.split(extra_args),
            ]
        )


@app.cls(
    image=endpoint_image,
    gpu=GPU,
    volumes={GLIMMER_MOUNT: glimmer_volume, DATA_MOUNT: data_volume},
    secrets=[
        modal.Secret.from_name(ENDPOINT_SECRET),
        modal.Secret.from_dict(
            {PROJECTOR_RUN_ENV: os.environ.get(PROJECTOR_RUN_ENV, DEFAULT_PROJECTOR_RUN)}
        ),
    ],
    cpu=ENDPOINT_CPUS,
    scaledown_window=ENDPOINT_IDLE_SECONDS,
    max_containers=1,
    timeout=HOUR,
)
@modal.concurrent(max_inputs=8)
class Endpoint:
    """The chat endpoint on one H100; the engine's lock keeps generation one request at a time."""

    @modal.enter()
    def load(self) -> None:
        """Loads the models once per container and logs how long that took."""
        import argparse

        from thalvi_train.serve import API_KEY_ENV, create_app, load_engine

        started = time.monotonic()
        run = os.environ[PROJECTOR_RUN_ENV]
        args = argparse.Namespace(
            glimmer=Path(GLIMMER_TEXT_DIR),
            projector=Path(RUNS_DIR) / run,
            whisper_model=WHISPER_MODEL,
            device="cuda",
            quantize=False,
            encoder_device="cuda",
            reply_mode="echo",
        )
        engine = load_engine(args)
        self.app = create_app(engine, api_key=os.environ[API_KEY_ENV])
        print(
            f"[endpoint] models loaded in {time.monotonic() - started:.1f} s "
            f"(projector {run}, {_utc_now()})",
            flush=True,
        )

    @modal.asgi_app()
    def web(self):
        """The Starlette app of `thalvi_train.serve`."""
        return self.app


@app.local_entrypoint()
def glimmer() -> None:
    """Step 1: download Glimmer onto `thalvi-glimmer`."""
    fetch_glimmer.remote()


@app.local_entrypoint()
def librispeech() -> None:
    """Step 2: download LibriSpeech onto `thalvi-data` and write its manifest."""
    fetch_librispeech.remote()


@app.local_entrypoint()
def corpora() -> None:
    """Step 2b: download VoxPopuli, FLEURS and MUSAN and pack them into `archives/`."""
    fetch_corpora.remote()


@app.local_entrypoint()
def teacher(
    limit: int = 0, temperature: float = 0.0, max_tokens: int = 512, concurrency: int = 64
) -> None:
    """Step 3: Glimmer answers every transcript; `--limit N` answers only the first N."""
    run_teacher.remote(limit, temperature, max_tokens, concurrency)


@app.local_entrypoint()
def train(run_name: str = "projector", extra_args: str = "") -> None:
    """Step 4: train the projector into `runs/<run_name>`; `--extra-args` goes to the trainer."""
    run_train.remote(run_name, extra_args)


@app.local_entrypoint()
def evaluate(
    run_name: str = "projector", limit: int = 500, batch_size: int = 32, extra_args: str = ""
) -> None:
    """Step 5: score `runs/<run_name>` on the first `--limit` eval samples, in batches."""
    run_eval.remote(run_name, limit, batch_size, extra_args)


@app.local_entrypoint()
def cascade(limit: int = 300, extra_args: str = "") -> None:
    """Baseline: Muse Voice Transcribe's word error rate on the first `--limit` eval clips."""
    run_cascade.remote(limit, extra_args)
