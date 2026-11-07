"""Oracle tests for fanout-kata-24 R13, `kata.rle.encode`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.rle import encode


def test_runs() -> None:
    assert encode("aaabcc") == "a3b1c2"


def test_empty_and_single() -> None:
    assert encode("") == ""
    assert encode("z") == "z1"
