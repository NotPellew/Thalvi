"""Owns local keyword detection using pre-existing ONNX model assets."""

import os

from .audio import samples
from .config import Config


class Wake:
    def __init__(self, config: Config):
        directory = config.model_dir
        stem = "epoch-12-avg-2-chunk-16-left-64.int8.onnx"
        names = [
            "tokens.txt",
            "bpe.model",
            *(f"{part}-{stem}" for part in ("encoder", "decoder", "joiner")),
        ]
        if directory is None or not all((directory / name).is_file() for name in names):
            raise RuntimeError(
                "Configure an existing sherpa-onnx keyword model directory; "
                "weights are not downloaded."
            )
        try:
            import sentencepiece as spm
            import sherpa_onnx

            tokenizer = spm.SentencePieceProcessor(model_file=str(directory / "bpe.model"))
            valid = {
                line.split()[0]
                for line in (directory / "tokens.txt").read_text().splitlines()
                if line.split()
            }
            lines = []
            for phrase, mode in (
                (config.capture_phrase, "capture"),
                (config.conversation_phrase, "conversation"),
            ):
                tokens = tokenizer.encode(phrase, out_type=str)
                if not tokens or any(token == "<unk>" or token not in valid for token in tokens):
                    raise ValueError(f"Unsupported wake phrase: {phrase}")
                lines.append(" ".join(tokens) + " @" + mode)
            self.keywords = "\n".join(lines) + "\n"
            fd = os.memfd_create("thalvi-keywords")
            try:
                os.write(fd, self.keywords.encode())
                self.model = sherpa_onnx.KeywordSpotter(
                    tokens=str(directory / names[0]),
                    encoder=str(directory / names[2]),
                    decoder=str(directory / names[3]),
                    joiner=str(directory / names[4]),
                    keywords_file=f"/proc/self/fd/{fd}",
                    num_threads=1,
                    provider="cpu",
                )
            finally:
                os.close(fd)
            self.reset()
        except ImportError as error:
            raise RuntimeError(
                "Install the desktop audio extra before loading wake models."
            ) from error
        except ValueError:
            raise
        except Exception as error:
            raise RuntimeError(
                "Wake model could not load. Check the model files and phrases."
            ) from error

    def detect(self, frame: bytes) -> str | None:
        try:
            import numpy as np

            if len(frame) != 7680:
                raise ValueError("Expected an 80 ms 48 kHz PCM frame.")
            pcm = samples(frame, 3).astype(np.float32) / 32768.0
            self.stream.accept_waveform(16000, pcm)
            while self.model.is_ready(self.stream):
                self.model.decode_stream(self.stream)
                keyword = self.model.keyword_spotter.get_result(self.stream).keyword.strip()
                if keyword:
                    self.reset()
                    return {"capture": "capture", "conversation": "conversation"}.get(keyword)
            return None
        except Exception as error:
            raise RuntimeError(
                "Wake detection failed. Check the configured model and audio."
            ) from error

    def reset(self) -> None:
        self.stream = self.model.create_stream()
