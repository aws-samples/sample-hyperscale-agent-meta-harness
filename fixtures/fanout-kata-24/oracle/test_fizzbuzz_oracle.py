"""Oracle tests for fanout-kata-24 R8, `kata.fizzbuzz.fizzbuzz`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.fizzbuzz import fizzbuzz


def test_first_five() -> None:
    assert fizzbuzz(5) == ["1", "2", "Fizz", "4", "Buzz"]


def test_fifteen_and_zero() -> None:
    assert fizzbuzz(15)[-1] == "FizzBuzz"
    assert fizzbuzz(0) == []
