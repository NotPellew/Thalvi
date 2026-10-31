"""Owns local ONNX wake detection; model files must already exist."""

from .audio import samples
from .config import Config


class Wake:
    def __init__(self, config: Config):
        models = (config.capture_model, config.conversation_model)
        if any(path is None or not path.is_file() for path in models):
            raise RuntimeError(
                "Configure two existing ONNX wake model files; "
                "no weights are downloaded automatically."
            )
        if config.model_dir is None:
            raise RuntimeError("wake.model_dir must contain the ONNX feature models.")
        feature_paths = [
            config.model_dir / name for name in ("melspectrogram.onnx", "embedding_model.onnx")
        ]
        if not all(path.is_file() for path in feature_paths):
            raise RuntimeError("wake.model_dir needs melspectrogram.onnx and embedding_model.onnx.")
        if models[0].stem == models[1].stem:
            raise RuntimeError("The two wake models must have distinct file names.")
        try:
            from openwakeword.model import Model
        except ImportError as error:
            raise RuntimeError(
                "Install the desktop audio extra before loading wake models."
            ) from error
        try:
            self.model = Model(
                wakeword_models=[str(path) for path in models],
                inference_framework="onnx",
                melspec_model_path=str(feature_paths[0]),
                embedding_model_path=str(feature_paths[1]),
            )
        except Exception as error:
            raise RuntimeError(
                "Wake models could not load. Check the ONNX files and feature models."
            ) from error
        self.names = {models[0].stem: "capture", models[1].stem: "conversation"}
        self.threshold = config.threshold

    def detect(self, frame: bytes) -> str | None:
        try:
            predictions = self.model.predict(samples(frame, 3))
            scores = [(float(predictions[name]), mode) for name, mode in self.names.items()]
        except Exception as error:
            raise RuntimeError(
                "Wake detection failed. Check the configured models and audio."
            ) from error
        score, mode = max(scores)
        return mode if score >= self.threshold else None

    def reset(self) -> None:
        self.model.reset()
