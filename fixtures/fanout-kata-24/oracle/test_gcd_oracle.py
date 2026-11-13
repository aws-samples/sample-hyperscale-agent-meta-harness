"""Oracle tests for fanout-kata-24 R17, `kata.gcd.gcd`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.gcd import gcd


def test_values() -> None:
    assert gcd(12, 18) == 6
    assert gcd(-4, 6) == 2


def test_zeros() -> None:
    assert gcd(0, 0) == 0
    assert gcd(0, 7) == 7
