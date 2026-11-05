"""Runs NVIDIA's unified Nemotron ASR as live streams through NeMo's buffered RNNT pipeline."""

from __future__ import annotations

import itertools
import re
from pathlib import Path

import numpy as np
import torch
from nemo.collections.asr.inference.factory.pipeline_builder import PipelineBuilder
from nemo.collections.asr.inference.model_wrappers.asr_inference_wrapper import ASRInferenceWrapper
from nemo.collections.asr.inference.streaming.framing.request import Frame
from nemo.collections.asr.inference.streaming.framing.request_options import ASRRequestOptions
from nemo.collections.asr.models import ASRModel
from omegaconf import OmegaConf

from thalvi_asr import STEP_SAMPLES, Step

CONFIG = Path(__file__).with_name("pipeline.yaml")
# Left context, chunk and right context in 80 ms encoder frames: 5.6 s, 0.16 s, 0.40 s, as in pipeline.yaml.
ATTENTION = [70, 2, 5]
LANGUAGE_TAG = re.compile(r"\s*<[a-z]{2}(-[A-Z]{2})?>")


def _load_half(model_name: str, map_location: torch.device) -> ASRModel:
    # The GPU is shared with Thalvi's TTS: NeMo's loader puts fp32 weights on it first and runs out of memory.
    model = ASRModel.restore_from(model_name, map_location=torch.device("cpu"))
    return model.to(torch.float16).to(map_location).eval()


class NemoModel:
    """The loaded model; each `open` starts an independent stream on the shared pipeline."""

    def __init__(self, model_path: str, endpoint_ms: int) -> None:
        ASRInferenceWrapper.load_model = staticmethod(_load_half)
        cfg = OmegaConf.load(CONFIG)
        cfg.asr.model_name = model_path
        cfg.endpointing.stop_history_eou = endpoint_ms
        self.endpoint_ms = endpoint_ms
        self.pipeline = PipelineBuilder.build_pipeline(cfg)
        # The unified model reaches its accuracy only with chunked attention matching the buffer.
        self.pipeline.asr_model.asr_model.encoder.set_default_att_context_size(att_context_size=ATTENTION)
        self.pipeline.open_session()
        self.ids = itertools.count()

    def open(self) -> NemoStream:
        return NemoStream(self, next(self.ids))


class NemoStream:
    def __init__(self, model: NemoModel, stream_id: int) -> None:
        self.model = model
        self.stream_id = stream_id
        self.first = True

    def step(self, samples: np.ndarray, last: bool) -> Step:
        options = None
        if self.first:
            options = ASRRequestOptions(
                enable_itn=False, asr_output_granularity="segment", stop_history_eou=self.model.endpoint_ms
            )
        frame = Frame(
            samples=torch.from_numpy(np.ascontiguousarray(samples[:STEP_SAMPLES], dtype=np.float32)),
            stream_id=self.stream_id,
            is_first=self.first,
            is_last=last,
            options=options,
        )
        self.first = False
        out = self.model.pipeline.transcribe_step([frame])[0]
        return Step(
            partial=LANGUAGE_TAG.sub("", out.partial_transcript), final=LANGUAGE_TAG.sub("", out.final_transcript)
        )
