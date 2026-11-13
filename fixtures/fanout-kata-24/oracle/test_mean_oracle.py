"""Oracle tests for fanout-kata-24 R18, `kata.mean.mean`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.mean import mean


def test_mean_is_a_float() -> None:
    assert mean([1, 2, 3, 4]) == 2.5
    assert mean([2]) == 2.0


def test_empty_raises() -> None:
    try:
        mean([])
    except ValueError:
        return
    raise AssertionError('mean([]) must raise ValueError')
