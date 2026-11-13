"""Oracle tests for fanout-kata R1. Owned by the E2E harness: they never enter a bundle, a
prompt or the hidden-test set. Plain asserts, so the fixture self-test runs them without pytest."""
from kata.strings import slugify


def test_words_and_punctuation() -> None:
    assert slugify("Hello, World!") == "hello-world"


def test_runs_collapse_and_ends_strip() -> None:
    assert slugify("  --Fan  Out--Kata 2  ") == "fan-out-kata-2"
