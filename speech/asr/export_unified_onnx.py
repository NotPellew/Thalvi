"""Exports Nemotron unified ASR 0.6B to the model folder Thalvi's desktop app runs on its CPU or, with --fp16, its GPU.

Run in `speech/asr` on the checkpoint that `speech/nemotron-asr.sh` pins:
    uv run --frozen --extra nemo --with onnx --with onnxruntime --with onnxscript \
        python export_unified_onnx.py [--fp16] nemotron-labs-unified-asr-0.6b.nemo OUTPUT_FOLDER

The folder's files are the assets of `unified_160ms` in `crates/thalvi-desktop/src/models.rs`, published with the
checkpoint's OpenMDW-1.1 `LICENSE.md`: the int8 export as the `speech-models-v1` release, the --fp16 export (an fp16
encoder with external data, fp32 decoder and joiner) as `speech-models-v2`. The int8 files cannot use a GPU: a third of
their operators run only on the CPU.
"""

from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import onnx
import torch
from nemo.collections.asr.models import ASRModel
from onnxruntime.quantization import QuantType, quantize_dynamic
from onnxruntime.transformers.float16 import convert_float_to_float16

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


def export(model_path: str, out: Path, fp16: bool) -> None:
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
    if not fp16:
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
        if fp16:
            shutil.copy(work / f"{name}.onnx", out / f"{name}.fp32.onnx")
        else:
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
    if fp16:
        encoder = onnx.load(str(work / "encoder.onnx"))
        # Inputs and outputs stay fp32, so the app feeds both exports alike.
        encoder = convert_float_to_float16(encoder, keep_io_types=True, disable_shape_infer=True)
    else:
        encoder = onnx.load(str(work / "encoder.q.onnx"))
    del encoder.metadata_props[:]
    for key, value in metadata.items():
        entry = encoder.metadata_props.add()
        entry.key, entry.value = key, str(value)
    if fp16:
        # The data file's name is written into the model, which finds it beside itself; the app keeps both together.
        onnx.save(
            encoder,
            str(out / "encoder.fp16.onnx"),
            save_as_external_data=True,
            all_tensors_to_one_file=True,
            location="encoder.fp16.onnx.data",
            size_threshold=1024,
        )
    else:
        onnx.save(encoder, str(out / "encoder.int8.onnx"))

    vocabulary = list(asr.joint.vocabulary)
    with open(out / "tokens.txt", "w", encoding="utf-8") as tokens:
        for index, piece in enumerate(vocabulary):
            tokens.write(f"{piece} {index}\n")
        tokens.write(f"<blk> {len(vocabulary)}\n")
    shutil.rmtree(work)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fp16", action="store_true", help="export for the GPU: fp16 encoder, fp32 decoder and joiner")
    parser.add_argument("checkpoint")
    parser.add_argument("output", type=Path)
    arguments = parser.parse_args()
    export(arguments.checkpoint, arguments.output, arguments.fp16)
