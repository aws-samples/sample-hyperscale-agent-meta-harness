"""Oracle tests for fanout-kata-24 R15, `kata.digits.digit_sum`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.digits import digit_sum


def test_positive_and_negative() -> None:
    assert digit_sum(1234) == 10
    assert digit_sum(-505) == 10


def test_zero() -> None:
    assert digit_sum(0) == 0
