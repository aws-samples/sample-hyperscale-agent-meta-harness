"""Oracle tests for fanout-kata-24 R20, `kata.binary.to_binary`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.binary import to_binary


def test_values() -> None:
    assert to_binary(10) == "1010"
    assert to_binary(0) == "0"


def test_negative_raises() -> None:
    try:
        to_binary(-1)
    except ValueError:
        return
    raise AssertionError('to_binary(-1) must raise ValueError')
