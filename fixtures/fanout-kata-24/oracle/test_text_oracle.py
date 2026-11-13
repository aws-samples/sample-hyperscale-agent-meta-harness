"""Oracle tests for fanout-kata-24 R4, `kata.text.word_count`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.text import word_count


def test_counts() -> None:
    assert word_count("The cat and the hat") == {"the": 2, "cat": 1, "and": 1, "hat": 1}


def test_empty_and_whitespace() -> None:
    assert word_count("") == {}
    assert word_count("a\tA\nb") == {"a": 2, "b": 1}
