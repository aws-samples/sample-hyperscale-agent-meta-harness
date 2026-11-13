"""Oracle tests for fanout-kata R3; see test_strings_oracle.py."""
from kata.lists import chunk


def test_chunks_keep_order() -> None:
    assert chunk([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]
    assert chunk([], 3) == []


def test_size_below_one_raises() -> None:
    try:
        chunk([1], 0)
    except ValueError:
        return
    raise AssertionError("chunk([1], 0) must raise ValueError")
