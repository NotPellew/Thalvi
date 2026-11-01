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


def test_ending_armed_during_speech_waits_for_quiet():
    pending = Pending(Capture(), 2.5)
    pending.final("hello paste that", 10, speech=True)
    assert pending.ready(100) is None
    pending.waiting_for_quiet = False
    pending.since = 100
    assert pending.ready(102.499) is None
    assert pending.ready(102.5).text == "hello"


def test_trailing_artifacts_do_not_restart_or_cancel_the_quiet_interval():
    pending = Pending(Capture(), 2.5)
    pending.final("hello submit", 10)
    for text in ("[noise] uh", "(breathing)", "<|nospeech|>"):
        pending.final(text, 11)
    assert pending.ready(12.499) is None
    request = pending.ready(12.5)
    assert request is not None
    assert request.text == "hello"


def test_artifacts_after_intentional_continuation_do_not_revive_an_ending():
    pending = Pending(Capture(), 2.5)
    pending.final("hello submit", 10)
    pending.continuation()
    pending.final("[noise]", 11)
    assert pending.ready(20) is None
    assert pending.capture.text == "hello submit"
