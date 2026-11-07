"""Oracle tests for fanout-kata-24 R24, `kata.hamming.distance`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.hamming import distance


def test_distance() -> None:
    assert distance("karolin", "kathrin") == 3
    assert distance("", "") == 0


def test_unequal_lengths_raise() -> None:
    try:
        distance("a", "ab")
    except ValueError:
        return
    raise AssertionError('distance("a", "ab") must raise ValueError')
