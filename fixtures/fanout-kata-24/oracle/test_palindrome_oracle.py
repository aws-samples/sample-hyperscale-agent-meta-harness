"""Oracle tests for fanout-kata-24 R9, `kata.palindrome.is_palindrome`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.palindrome import is_palindrome


def test_phrase() -> None:
    assert is_palindrome("A man, a plan, a canal: Panama") is True


def test_not_a_palindrome() -> None:
    assert is_palindrome("hello") is False
    assert is_palindrome("") is True
