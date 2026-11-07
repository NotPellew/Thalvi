"""Exports Nemotron unified ASR 0.6B to the model folder Thalvi's desktop app runs on its CPU.

Run in `speech/asr` on the checkpoint that `speech/nemotron-asr.sh` pins:
    uv run --frozen --extra nemo --with onnx --with onnxruntime --with onnxscript \
        python export_unified_onnx.py nemotron-labs-unified-asr-0.6b.nemo OUTPUT_FOLDER

The folder's files are the assets of `unified_160ms` in `crates/thalvi-desktop/src/models.rs`, published in the
`speech-models-v1` release with the checkpoint's OpenMDW-1.1 `LICENSE.md`.
"""

from __future__ import annotations

import json
import shutil
import sys
from pathlib import Path

import onnx
import torch
from nemo.collections.asr.models import ASRModel
from onnxruntime.quantization import QuantType, quantize_dynamic

LEFT_S, CHUNK_S, RIGHT_S = 5.6, 0.16, 0.40
# Left context, chunk and right context in 80 ms encoder frames, as in pipeline.yaml.
ATTENTION = [70, 2, 5]
FEATURE_FRAME_S = 0.01
FRAMES = int((LEFT_S + CHUNK_S + RIGHT_S) / FEATURE_FRAME_S)


class PromptedEncoder(torch.nn.Module):
    """The encoder with the language prompt fused into its output, as the model does inside its own forward."""

    def __init__(self, asr: ASRModel) -> None:
        super().__init__()
        self.encoder = asr.encoder
        self.kernel = asr.prompt_kernel
        self.num_prompts = asr.num_prompts

    def forward(self, features: torch.Tensor, length: torch.Tensor, prompt_index: torch.Tensor):
        encoded, encoded_len = self.encoder.forward_internal(audio_signal=features, length=length)
        encoded = encoded.transpose(1, 2)
        one_hot = torch.nn.functional.one_hot(prompt_index, self.num_prompts).to(encoded.dtype)
        one_hot = one_hot[:, None, :].expand(-1, encoded.shape[1], -1)
        fused = self.kernel(torch.cat([encoded, one_hot], dim=-1))
        return fused.transpose(1, 2), encoded_len


def export(model_path: str, out: Path) -> None:
    work = out / "work"
    work.mkdir(parents=True, exist_ok=True)
    asr = ASRModel.restore_from(model_path, map_location=torch.device("cpu")).eval()
    asr.encoder.set_default_att_context_size(att_context_size=ATTENTION)
    auto = asr.resolve_prompt_language(None)

    features = torch.randn(1, asr.cfg.preprocessor.features, FRAMES)
    asr.encoder.update_max_seq_length(seq_length=FRAMES, device=features.device)
    torch.onnx.export(
        PromptedEncoder(asr).eval(),
        (features, torch.tensor([FRAMES]), torch.tensor([auto])),
        str(work / "encoder.onnx"),
        input_names=["features", "length", "prompt_index"],
        output_names=["encoded", "encoded_length"],
        opset_version=17,
        dynamo=False,
        external_data=True,
    )
    # MatMul only: it keeps the encoder a single file under ONNX's 2 GB limit at the word errors of fp32.
    quantize_dynamic(
        str(work / "encoder.onnx"),
        str(work / "encoder.q.onnx"),
        weight_type=QuantType.QUInt8,
        op_types_to_quantize=["MatMul"],
        use_external_data_format=False,
    )

    asr.set_export_config({"cache_support": False})
    asr.decoder.export(str(work / "decoder.onnx"))
    asr.joint.export(str(work / "joiner.onnx"))
    for name in ("decoder", "joiner"):
        quantize_dynamic(str(work / f"{name}.onnx"), str(out / f"{name}.int8.onnx"), weight_type=QuantType.QUInt8)

    metadata = {
        "model_type": "UnifiedBufferedRNNT",
        "url": "https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b",
        "license": "OpenMDW-1.1",
        "feat_dim": 128,
        "window_size": FRAMES,
        "chunk_shift": round(CHUNK_S / FEATURE_FRAME_S),
        "right_context": round(RIGHT_S / FEATURE_FRAME_S),
        "subsampling_factor": 8,
        "vocab_size": asr.decoder.vocab_size,
        "pred_rnn_layers": asr.decoder.pred_rnn_layers,
        "pred_hidden": asr.decoder.pred_hidden,
        "prompt_auto": auto,
        "prompts": json.dumps(dict(asr.cfg.model_defaults.get("prompt_dictionary") or {}), sort_keys=True),
    }
    encoder = onnx.load(str(work / "encoder.q.onnx"))
    del encoder.metadata_props[:]
    for key, value in metadata.items():
        entry = encoder.metadata_props.add()
        entry.key, entry.value = key, str(value)
    onnx.save(encoder, str(out / "encoder.int8.onnx"))

    vocabulary = list(asr.joint.vocabulary)
    with open(out / "tokens.txt", "w", encoding="utf-8") as tokens:
        for index, piece in enumerate(vocabulary):
            tokens.write(f"{piece} {index}\n")
        tokens.write(f"<blk> {len(vocabulary)}\n")
    shutil.rmtree(work)


if __name__ == "__main__":
    export(sys.argv[1], Path(sys.argv[2]))
