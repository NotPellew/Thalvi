from thalvi_train.engine import StepTimer


def clock(times: list[float]):
    return lambda: times.pop(0)


def test_the_summary_names_prefill_rate_and_slowest_step() -> None:
    timer = StepTimer(clock([0.0, 1.0, 1.1, 1.2, 2.2]))
    for _ in range(4):
        timer.mark()
    assert timer.summary(30) == (
        "[engine] prompt 30 tokens, prefill 1.00 s, 3 steps at 2.5/s, slowest 1.00 s at step 3"
    )


def test_a_request_cancelled_before_its_first_token_says_so() -> None:
    assert "cancelled" in StepTimer(clock([0.0])).summary(5)


def test_the_summary_separates_building_the_prompt_from_the_prefill() -> None:
    timer = StepTimer(clock([0.0, 0.8, 1.0, 1.5]))
    timer.built()
    timer.mark()
    timer.mark()
    assert timer.summary(30) == (
        "[engine] prompt 30 tokens built in 0.80 s, prefill 0.20 s,"
        " 1 steps at 2.0/s, slowest 0.50 s at step 1"
    )
