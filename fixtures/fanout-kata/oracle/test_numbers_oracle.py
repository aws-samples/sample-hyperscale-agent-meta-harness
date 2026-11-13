"""Oracle tests for fanout-kata R2; see test_strings_oracle.py."""
from kata.numbers import clamp


def test_above_and_below() -> None:
    assert clamp(5, 0, 3) == 3
    assert clamp(-1, 0, 3) == 0
    assert clamp(2, 0, 3) == 2


def test_inverted_bounds_raise() -> None:
    try:
        clamp(1, 3, 0)
    except ValueError:
        return
    raise AssertionError("clamp(1, 3, 0) must raise ValueError")
