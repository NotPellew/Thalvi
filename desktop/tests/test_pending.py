from thalvi_desktop.capture import Capture, Ending
from thalvi_desktop.pending import Pending


def test_final_ending_requires_continuous_pause_and_preserves_continuation():
    current = Capture()
    pending = Pending(current, 2.5)
    assert pending.final("hello submit", 10) is None
    assert pending.ready(12.499) is None
    pending.continuation()
    assert pending.ready(12.5) is None
    assert pending.final("more words submit", 13) is None
    request = pending.ready(15.5)
    assert request is not None
    assert request.ending == Ending.SUBMIT
    assert request.text == "hello submit more words"
    assert request.pause_confirmed


def test_split_ending_and_immediate_cancel():
    current = Capture()
    pending = Pending(current, 2.5)
    assert pending.final("hello paste", 0) is None
    assert pending.final("that", 1) is None
    assert pending.ready(3.5).text == "hello"
    pending = Pending(Capture(), 2.5)
    assert pending.final("never mind", 0).ending == Ending.CANCEL


def test_empty_final_does_not_revive_cancelled_ending():
    pending = Pending(Capture(), 2.5)
    pending.final("hello submit", 10)
    pending.continuation()
    pending.final("  ", 11)
    assert pending.ready(13.5) is None
    pending.final("more words", 14)
    assert pending.request is None
    assert pending.capture.text == "hello submit more words"


def test_scratch_preserves_previous_final_after_continuation():
    pending = Pending(Capture(), 2.5)
    pending.final("first piece", 0)
    pending.final("second submit", 1)
    pending.continuation()
    pending.final("scratch that", 2)
    assert pending.capture.text == "first piece"
