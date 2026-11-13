"""Oracle tests for fanout-kata-24 R19, `kata.median.median`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.median import median


def test_odd_and_even() -> None:
    assert median([3, 1, 2]) == 2
    assert median([4, 1, 3, 2]) == 2.5


def test_empty_raises() -> None:
    try:
        median([])
    except ValueError:
        return
    raise AssertionError('median([]) must raise ValueError')
