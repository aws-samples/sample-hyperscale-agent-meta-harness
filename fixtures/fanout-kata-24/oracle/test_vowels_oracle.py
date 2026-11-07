"""Oracle tests for fanout-kata-24 R10, `kata.vowels.count_vowels`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.vowels import count_vowels


def test_counts_any_case() -> None:
    assert count_vowels("Programming is fun") == 5
    assert count_vowels("AEIOU") == 5


def test_none() -> None:
    assert count_vowels("xyz") == 0
