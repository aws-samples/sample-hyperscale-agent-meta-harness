"""Oracle tests for fanout-kata-24 R21, `kata.leap.is_leap`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.leap import is_leap


def test_centuries() -> None:
    assert is_leap(1900) is False
    assert is_leap(2000) is True


def test_ordinary_years() -> None:
    assert is_leap(2024) is True
    assert is_leap(2023) is False
