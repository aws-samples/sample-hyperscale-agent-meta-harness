"""Oracle tests for fanout-kata-24 R14, `kata.anagram.is_anagram`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.anagram import is_anagram


def test_ignores_case_and_spaces() -> None:
    assert is_anagram("Dormitory", "Dirty room") is True


def test_different_letters() -> None:
    assert is_anagram("abc", "abd") is False
    assert is_anagram("aab", "ab") is False
