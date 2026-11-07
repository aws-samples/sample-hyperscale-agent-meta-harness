"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def fib(n: int) -> int:
    if n < 0:
        raise ValueError("n < 0")
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a
