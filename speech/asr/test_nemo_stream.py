"""The recognizer stream's predictor reset after long digital silence, driven through a fake pipeline."""

from types import SimpleNamespace

import numpy as np
import pytest

torch = pytest.importorskip("torch")
nemo_stream = pytest.importorskip("nemo_stream")
from nemo_stream import SILENCE_RESET_STEPS, NemoStream  # noqa: E402
from thalvi_asr import STEP_SAMPLES  # noqa: E402

STALE = {
    "predictor_state": (torch.tensor([9.0]),),
    "predictor_output": torch.tensor([9.0]),
    "label": torch.tensor([3936]),
}
FRESH = {
    "predictor_state": (torch.tensor([1.0]),),
    "predictor_output": torch.tensor([1.0]),
    "label": torch.tensor([4095]),
}


class FakePipeline:
    def __init__(self, finals: list[str]) -> None:
        self.finals = finals
        self.calls = 0
        self.hyp = SimpleNamespace(
            predictor_state=(torch.tensor([9.0]),),
            predictor_output=torch.tensor([9.0]),
            label=torch.tensor([3936]),
            decoded_length=torch.tensor([376]),
        )
        self.state = SimpleNamespace(hyp_decoding_state=self.hyp)

    def transcribe_step(self, frames):
        final = self.finals[self.calls] if self.calls < len(self.finals) else ""
        self.calls += 1
        return [SimpleNamespace(partial_transcript="", final_transcript=final)]

    def get_state(self, stream_id: int):
        return self.state


def stream(finals: list[str]) -> NemoStream:
    model = SimpleNamespace(pipeline=FakePipeline(finals), endpoint_ms=600, fresh=FRESH)
    return NemoStream(model, 0, ())


def test_the_predictor_resets_once_digital_silence_flushes_the_left_context_after_a_final():
    s = stream([" Done."])
    s.step(np.ones(STEP_SAMPLES, np.float32), False)
    hyp = s.model.pipeline.hyp
    for _ in range(SILENCE_RESET_STEPS - 1):
        s.step(np.zeros(STEP_SAMPLES, np.float32), False)
    assert hyp.label.item() == STALE["label"].item()
    s.step(np.zeros(STEP_SAMPLES, np.float32), False)
    assert hyp.label.item() == FRESH["label"].item()
    assert hyp.predictor_output.item() == FRESH["predictor_output"].item()
    assert hyp.predictor_state[0].item() == FRESH["predictor_state"][0].item()
    assert hyp.decoded_length.item() == 376
    hyp.predictor_output.fill_(7.0)
    assert FRESH["predictor_output"].item() == 1.0


@pytest.mark.parametrize(
    ("finals", "steps"),
    [
        ([""], SILENCE_RESET_STEPS),
        ([" Done."], SILENCE_RESET_STEPS - 1),
    ],
)
def test_no_reset_without_a_final_or_before_the_left_context_is_flushed(finals, steps):
    s = stream(finals)
    s.step(np.ones(STEP_SAMPLES, np.float32), False)
    for _ in range(steps):
        s.step(np.zeros(STEP_SAMPLES, np.float32), False)
    assert s.model.pipeline.hyp.label.item() == STALE["label"].item()


def test_no_reset_while_sound_continues_after_a_final():
    s = stream([" Done."])
    s.step(np.ones(STEP_SAMPLES, np.float32), False)
    for _ in range(SILENCE_RESET_STEPS):
        s.step(np.full(STEP_SAMPLES, 0.5, np.float32), False)
    assert s.model.pipeline.hyp.label.item() == STALE["label"].item()
