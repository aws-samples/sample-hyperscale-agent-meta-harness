"""Oracle tests for fanout-kata-24 R1, `kata.strings.slugify`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set. Plain asserts, so the self-test runs
them without pytest; the README fixes the semantics they check."""
from kata.strings import slugify


def test_punctuation_and_case() -> None:
    assert slugify("Hello, World!") == "hello-world"


def test_runs_and_ends() -> None:
    assert slugify("  --A--b  ") == "a-b"
    assert slugify("3 Little Pigs") == "3-little-pigs"
