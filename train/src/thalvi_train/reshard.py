"""Text-only copy of a Glimmer checkpoint in small shards, so loading never maps one huge file.

transformers maps each shard copy-on-write, and Linux's default overcommit
heuristic refuses a single mapping larger than RAM plus swap: Glimmer's 50 GB
first shard fails on a 32 GB machine. Run as
`python -m thalvi_train.reshard <checkpoint> <out_dir> [--shard-gb 4]`. Tensors
are read with `pread`, one at a time, and only the decoder and `lm_head` are
kept. The weight index is written last, so a finished copy is one whose index
exists.
"""

import argparse
import json
import shutil
from collections.abc import Iterator
from pathlib import Path

from safetensors import safe_open
from safetensors.torch import save_file

from thalvi_train.glimmer import LM_HEAD_KEY

INDEX = "model.safetensors.index.json"
SINGLE_FILE = "model.safetensors"
TEXT_PREFIX = "model.language_model."
DTYPE_BYTES = {"BF16": 2, "F16": 2, "F32": 4}


def _weight_map(checkpoint: Path) -> dict[str, str]:
    index = checkpoint / INDEX
    if index.exists():
        return json.loads(index.read_text())["weight_map"]
    with safe_open(checkpoint / SINGLE_FILE, framework="pt", backend="pread") as shard:
        return dict.fromkeys(shard.keys(), SINGLE_FILE)


def _text_tensors(checkpoint: Path) -> Iterator[tuple[str, str, int]]:
    """Yields `(key, shard, bytes)` for every kept tensor, in checkpoint order."""
    by_shard: dict[str, list[str]] = {}
    for key, shard in _weight_map(checkpoint).items():
        if key.startswith(TEXT_PREFIX) or key == LM_HEAD_KEY:
            by_shard.setdefault(shard, []).append(key)
    for shard, keys in sorted(by_shard.items()):
        with safe_open(checkpoint / shard, framework="pt", backend="pread") as reader:
            for key in sorted(keys):
                tensor = reader.get_slice(key)
                elements = 1
                for size in tensor.get_shape():
                    elements *= size
                yield key, shard, elements * DTYPE_BYTES[tensor.get_dtype()]


def plan_shards(checkpoint: Path, shard_bytes: int) -> list[list[tuple[str, str]]]:
    """Groups kept tensors into output shards of at most `shard_bytes` (or one tensor)."""
    shards: list[list[tuple[str, str]]] = [[]]
    used = 0
    for key, source, size in _text_tensors(checkpoint):
        if shards[-1] and used + size > shard_bytes:
            shards.append([])
            used = 0
        shards[-1].append((key, source))
        used += size
    return shards


def reshard(checkpoint: Path, out_dir: Path, shard_bytes: int) -> None:
    """Writes the text-only, resharded copy of `checkpoint` to `out_dir`."""
    if (out_dir / INDEX).exists():
        print(f"{out_dir} is already complete")
        return
    out_dir.mkdir(parents=True, exist_ok=True)
    for extra in checkpoint.iterdir():
        if extra.is_file() and extra.suffix != ".safetensors" and extra.name != INDEX:
            shutil.copy2(extra, out_dir / extra.name)
    plan = plan_shards(checkpoint, shard_bytes)
    weight_map: dict[str, str] = {}
    total = 0
    for number, entries in enumerate(plan, start=1):
        name = f"model-{number:05d}-of-{len(plan):05d}.safetensors"
        tensors = {}
        for key, source in entries:
            with safe_open(checkpoint / source, framework="pt", backend="pread") as reader:
                tensors[key] = reader.get_tensor(key)
            weight_map[key] = name
        total += sum(t.numel() * t.element_size() for t in tensors.values())
        save_file(tensors, out_dir / f"{name}.part", metadata={"format": "pt"})
        (out_dir / f"{name}.part").rename(out_dir / name)
        print(json.dumps({"shard": name, "tensors": len(tensors)}), flush=True)
    index = {"metadata": {"total_size": total}, "weight_map": weight_map}
    (out_dir / f"{INDEX}.part").write_text(json.dumps(index, indent=2))
    (out_dir / f"{INDEX}.part").rename(out_dir / INDEX)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("checkpoint", type=Path)
    parser.add_argument("out_dir", type=Path)
    parser.add_argument("--shard-gb", type=float, default=4.0)
    args = parser.parse_args(argv)
    reshard(args.checkpoint, args.out_dir, int(args.shard_gb * 1024**3))


if __name__ == "__main__":
    main()
