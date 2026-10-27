"""Tiny parquet files shaped like the VoxPopuli and FLEURS ones, and a MUSAN-like tree."""

import io
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import soundfile
from tiny_whisper import tone


def wav_bytes(seconds: float, rate: int = 16_000, seed: int = 0) -> bytes:
    buffer = io.BytesIO()
    soundfile.write(buffer, tone(int(seconds * rate), seed), rate, format="WAV", subtype="FLOAT")
    return buffer.getvalue()


def audio_column(clips: list[bytes]) -> pa.Array:
    return pa.array(
        [{"bytes": clip, "path": "x.wav"} for clip in clips],
        pa.struct([("bytes", pa.binary()), ("path", pa.string())]),
    )


def write_voxpopuli(path: Path, rows: list[dict]) -> Path:
    """Rows hold `id`, `raw`, `normalized`, `speaker`, `seconds` and optionally `rate`."""
    table = pa.table(
        {
            "audio_id": [row["id"] for row in rows],
            "audio": audio_column(
                [
                    wav_bytes(row["seconds"], row.get("rate", 16_000), n)
                    for n, row in enumerate(rows)
                ]
            ),
            "raw_text": [row["raw"] for row in rows],
            "normalized_text": [row["normalized"] for row in rows],
            "speaker_id": [row["speaker"] for row in rows],
        }
    )
    pq.write_table(table, path, row_group_size=2)
    return path


def write_fleurs(path: Path, rows: list[dict]) -> Path:
    """Rows hold `stem`, `sentence`, `text`, `gender` and `seconds`."""
    table = pa.table(
        {
            "id": pa.array([row["sentence"] for row in rows], pa.int32()),
            "path": [f"/cache/{row['stem']}.wav" for row in rows],
            "audio": audio_column(
                [wav_bytes(row["seconds"], seed=n) for n, row in enumerate(rows)]
            ),
            "raw_transcription": [row["text"] for row in rows],
            "gender": pa.array([row["gender"] for row in rows], pa.int64()),
        }
    )
    pq.write_table(table, path, row_group_size=2)
    return path


def write_musan(root: Path) -> Path:
    """A `musan/` tree with two noise files, one music file, one speech file and non-audio files."""
    for name, seconds in (
        ("noise/free-sound/noise-0.wav", 1.0),
        ("noise/sound-bible/noise-1.wav", 2.5),
        ("music/fma/music-0.wav", 3.0),
        ("speech/librivox/speech-0.wav", 0.5),
    ):
        path = root / "musan" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        soundfile.write(path, np.zeros(int(seconds * 16_000), dtype=np.float32), 16_000)
    (root / "musan" / "README").write_text("MUSAN")
    (root / "musan" / "noise" / "ANNOTATIONS").write_text("x")
    return root / "musan"
