"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def digit_sum(n: int) -> int:
    return sum(int(d) for d in str(abs(n)))
