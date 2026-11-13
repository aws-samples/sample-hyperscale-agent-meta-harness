"""Oracle tests for fanout-kata R4; see test_strings_oracle.py."""
from kata.text import word_count


def test_counts_lowercased_words() -> None:
    assert word_count("The cat and the hat") == {"the": 2, "cat": 1, "and": 1, "hat": 1}
    assert word_count("") == {}
