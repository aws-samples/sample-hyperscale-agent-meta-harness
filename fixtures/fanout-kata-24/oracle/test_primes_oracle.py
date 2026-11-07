"""Oracle tests for fanout-kata-24 R7, `kata.primes.primes_up_to`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.primes import primes_up_to


def test_primes_to_twenty() -> None:
    assert primes_up_to(20) == [2, 3, 5, 7, 11, 13, 17, 19]


def test_below_two_is_empty() -> None:
    assert primes_up_to(1) == []
    assert primes_up_to(2) == [2]
