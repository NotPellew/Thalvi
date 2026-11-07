"""Runs NVIDIA's unified Nemotron ASR as live streams through NeMo's buffered RNNT pipeline."""

from __future__ import annotations

import copy
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
from nemo.collections.asr.parts.context_biasing.biasing_multi_model import BiasingRequestItemConfig
from nemo.collections.asr.parts.context_biasing.boosting_graph_batched import BoostingTreeModelConfig
from omegaconf import OmegaConf

from thalvi_asr import STEP_SAMPLES, Step

CONFIG = Path(__file__).with_name("pipeline.yaml")
# Left context, chunk and right context in 80 ms encoder frames: 5.6 s, 0.16 s, 0.40 s, as in pipeline.yaml.
ATTENTION = [70, 2, 5]
# The weight that turned "CyberX2" into a listed "Cybrex2" without changing ordinary takes (issue #214).
BOOST = 1.0
# Digital-silence steps after a final that flush the left context: 70 frames at 2 frames per step (issue #223).
SILENCE_RESET_STEPS = ATTENTION[0] // ATTENTION[1]
LANGUAGE_TAG = re.compile(r"\s*<[a-z]{2}(-[A-Z]{2})?>")


def _clone(value):
    if torch.is_tensor(value):
        return value.clone()
    if isinstance(value, tuple):
        return tuple(_clone(item) for item in value)
    if isinstance(value, list):
        return [_clone(item) for item in value]
    return copy.deepcopy(value)


def _load_on_cpu(model_name: str, map_location: torch.device) -> ASRModel:
    # The GPU is shared with Thalvi's TTS: NeMo's loader keeps the checkpoint on it beside the model and runs out.
    return ASRModel.restore_from(model_name, map_location=torch.device("cpu")).to(map_location).eval()


class NemoModel:
    """The loaded model; each `open` starts an independent stream on the shared pipeline."""

    def __init__(self, model_path: str, endpoint_ms: int) -> None:
        ASRInferenceWrapper.load_model = staticmethod(_load_on_cpu)
        cfg = OmegaConf.load(CONFIG)
        cfg.asr.model_name = model_path
        cfg.endpointing.stop_history_eou = endpoint_ms
        self.endpoint_ms = endpoint_ms
        self.pipeline = PipelineBuilder.build_pipeline(cfg)
        # The unified model reaches its accuracy only with chunked attention matching the buffer.
        self.pipeline.asr_model.asr_model.encoder.set_default_att_context_size(att_context_size=ATTENTION)
        self.pipeline.open_session()
        self.ids = itertools.count()
        self.fresh: dict | None = None
        virgin = self.open(())
        virgin.step(np.zeros(STEP_SAMPLES, np.float32), False)
        state = self.pipeline.get_state(virgin.stream_id)
        if state is not None and state.hyp_decoding_state is not None:
            hyp = state.hyp_decoding_state
            self.fresh = {name: _clone(getattr(hyp, name)) for name in ("predictor_state", "predictor_output", "label")}
        virgin.step(np.zeros(STEP_SAMPLES, np.float32), True)

    def open(self, phrases: tuple[str, ...]) -> NemoStream:
        return NemoStream(self, next(self.ids), phrases)


class NemoStream:
    def __init__(self, model: NemoModel, stream_id: int, phrases: tuple[str, ...]) -> None:
        self.model = model
        self.stream_id = stream_id
        self.phrases = phrases
        self.first = True
        self.silent_steps = 0
        self.saw_final = False

    def step(self, samples: np.ndarray, last: bool) -> Step:
        options = None
        if self.first:
            biasing = None
            if self.phrases:
                biasing = BiasingRequestItemConfig(
                    boosting_model_cfg=BoostingTreeModelConfig(key_phrases_list=list(self.phrases)),
                    boosting_model_alpha=BOOST,
                    # The same list comes with every capture; the cache builds its tree once.
                    cache_key="\n".join(self.phrases),
                )
            options = ASRRequestOptions(
                enable_itn=False,
                asr_output_granularity="segment",
                stop_history_eou=self.model.endpoint_ms,
                biasing_cfg=biasing,
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
        step = Step(
            partial=LANGUAGE_TAG.sub("", out.partial_transcript), final=LANGUAGE_TAG.sub("", out.final_transcript)
        )
        if step.final:
            self.saw_final = True
        self.silent_steps = self.silent_steps + 1 if not np.any(samples[:STEP_SAMPLES]) else 0
        if not last and self.saw_final and self.silent_steps >= SILENCE_RESET_STEPS and self.model.fresh is not None:
            state = self.model.pipeline.get_state(self.stream_id)
            if state is not None and state.hyp_decoding_state is not None:
                hyp = state.hyp_decoding_state
                for name, value in self.model.fresh.items():
                    setattr(hyp, name, _clone(value))
                self.silent_steps = 0
                self.saw_final = False
        return step
