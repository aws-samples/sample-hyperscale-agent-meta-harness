"""Oracle tests for fanout-kata-24 R12, `kata.dedupe.dedupe`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.dedupe import dedupe


def test_first_occurrence_order() -> None:
    assert dedupe([3, 1, 3, 2, 1]) == [3, 1, 2]


def test_strings_and_empty() -> None:
    assert dedupe(["a", "a", "b"]) == ["a", "b"]
    assert dedupe([]) == []
