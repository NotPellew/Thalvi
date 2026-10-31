"""Owns local wake inference using explicitly provisioned ONNX assets."""

import math
from numbers import Real
from typing import Literal

from .audio import samples
from .config import Config


class Wake:
    def __init__(self, config: Config):
        directory = config.model_dir
        capture = config.capture_model
        if directory is None or capture is None:
            raise RuntimeError("Configure the wake model directory and trained capture model.")
        mycroft = directory / "hey_mycroft_v0.1.onnx"
        mel = directory / "melspectrogram.onnx"
        embedding = directory / "embedding_model.onnx"
        if capture.stem == mycroft.stem:
            raise ValueError("Wake model head keys must be distinct.")
        if not all(path.is_file() for path in (capture, mycroft, mel, embedding)):
            raise RuntimeError("Wake model assets are missing; provision them before starting.")
        self.keys = (capture.stem, mycroft.stem)
        self.thresholds = (config.capture_threshold, config.conversation_threshold)
        try:
            from openwakeword.model import Model

            self.model = Model(
                wakeword_models=[str(capture), str(mycroft)],
                inference_framework="onnx",
                melspec_model_path=str(mel),
                embedding_model_path=str(embedding),
                ncpu=1,
                vad_threshold=0,
                enable_speex_noise_suppression=False,
            )
            for key in self.keys:
                session = self.model.models[key]
                inputs, outputs = session.get_inputs(), session.get_outputs()
                if len(inputs) != 1 or len(outputs) != 1:
                    raise ValueError("Wake head must have one input and output.")
                inp, out = inputs[0], outputs[0]

                def batch(dim):
                    return dim == 1 or dim is None or isinstance(dim, str)

                if (
                    inp.type != "tensor(float)"
                    or out.type != "tensor(float)"
                    or len(inp.shape) != 3
                    or len(out.shape) != 2
                    or not batch(inp.shape[0])
                    or not batch(out.shape[0])
                    or type(inp.shape[1]) is not int
                    or inp.shape[1] != 16
                    or inp.shape[2] != 96
                    or out.shape[1] != 1
                ):
                    raise ValueError("Wake head has an unsupported input or output shape.")
        except ImportError as error:
            raise RuntimeError(
                "Install the desktop audio extra before loading wake models."
            ) from error
        except Exception as error:
            raise RuntimeError("Wake model could not load or has invalid metadata.") from error

    def detect(self, frame: bytes) -> Literal["capture", "conversation", "ambiguous"] | None:
        try:
            if len(frame) != 7680:
                raise ValueError("Expected an 80 ms 48 kHz PCM frame.")
            predictions = self.model.predict(samples(frame, 3))
            scores = []
            for key in self.keys:
                value = predictions[key]
                if (
                    isinstance(value, bool)
                    or not isinstance(value, Real)
                    or not math.isfinite(value)
                    or not 0 <= value <= 1
                ):
                    raise ValueError("Invalid wake score.")
                scores.append(value)
            hits = [
                score >= threshold for score, threshold in zip(scores, self.thresholds, strict=True)
            ]
            if all(hits):
                return "ambiguous"
            if hits[0]:
                return "capture"
            if hits[1]:
                return "conversation"
            return None
        except Exception as error:
            raise RuntimeError(
                "Wake detection failed. Check the configured model and audio."
            ) from error

    def reset(self) -> None:
        self.model.reset()
