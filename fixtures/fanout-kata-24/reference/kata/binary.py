"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def to_binary(n: int) -> str:
    if n < 0:
        raise ValueError("n < 0")
    return format(n, "b")
