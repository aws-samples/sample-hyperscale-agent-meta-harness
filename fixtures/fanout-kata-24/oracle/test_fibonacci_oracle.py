"""Oracle tests for fanout-kata-24 R16, `kata.fibonacci.fib`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.fibonacci import fib


def test_values() -> None:
    assert fib(0) == 0
    assert fib(1) == 1
    assert fib(10) == 55


def test_negative_raises() -> None:
    try:
        fib(-1)
    except ValueError:
        return
    raise AssertionError('fib(-1) must raise ValueError')
