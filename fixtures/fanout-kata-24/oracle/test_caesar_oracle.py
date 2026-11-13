"""Oracle tests for fanout-kata-24 R5, `kata.caesar.shift`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.caesar import shift


def test_shift_keeps_case_and_punctuation() -> None:
    assert shift("Hello, World!", 3) == "Khoor, Zruog!"


def test_wraps_both_ways() -> None:
    assert shift("abc", -1) == "zab"
    assert shift("xyz", 29) == "abc"
