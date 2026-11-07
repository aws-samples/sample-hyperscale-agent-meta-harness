"""Oracle tests for fanout-kata-24 R11, `kata.flatten.flatten`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.flatten import flatten


def test_any_depth() -> None:
    assert flatten([1, [2, [3, [4]]], 5]) == [1, 2, 3, 4, 5]


def test_only_lists_open() -> None:
    assert flatten([]) == []
    assert flatten([(1, 2), ["a"]]) == [(1, 2), "a"]
