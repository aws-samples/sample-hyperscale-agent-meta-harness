"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def distance(a: str, b: str) -> int:
    if len(a) != len(b):
        raise ValueError("unequal lengths")
    return sum(1 for x, y in zip(a, b) if x != y)
