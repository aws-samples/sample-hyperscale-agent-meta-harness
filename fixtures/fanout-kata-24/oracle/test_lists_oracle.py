"""Oracle tests for fanout-kata-24 R3, `kata.lists.chunk`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.lists import chunk


def test_chunks() -> None:
    assert chunk([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]
    assert chunk([], 3) == []


def test_size_below_one_raises() -> None:
    try:
        chunk([1], 0)
    except ValueError:
        return
    raise AssertionError('chunk([1], 0) must raise ValueError')
