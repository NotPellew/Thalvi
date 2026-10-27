import json
from pathlib import Path

import torch

from thalvi_train.glimmer import load_glimmer
from thalvi_train.reshard import INDEX, reshard


def test_reshards_to_a_text_only_checkpoint_that_loads_identically(
    tiny_glimmer_dir: Path, tmp_path: Path
) -> None:
    out = tmp_path / "text-only"
    reshard(tiny_glimmer_dir, out, shard_bytes=4 * 1024**2)

    weight_map = json.loads((out / INDEX).read_text())["weight_map"]
    assert len(set(weight_map.values())) > 1
    assert not [key for key in weight_map if "vision" in key]
    assert (out / "config.json").exists()

    original, copy = load_glimmer(tiny_glimmer_dir), load_glimmer(out)
    for (name, a), (_, b) in zip(
        original.state_dict().items(), copy.state_dict().items(), strict=True
    ):
        assert torch.equal(a, b), name


def test_leaves_a_finished_copy_alone(tiny_glimmer_dir: Path, tmp_path: Path) -> None:
    out = tmp_path / "text-only"
    reshard(tiny_glimmer_dir, out, shard_bytes=4 * 1024**2)
    stamps = {p.name: p.stat().st_mtime_ns for p in out.iterdir()}
    reshard(tiny_glimmer_dir, out, shard_bytes=4 * 1024**2)
    assert stamps == {p.name: p.stat().st_mtime_ns for p in out.iterdir()}
