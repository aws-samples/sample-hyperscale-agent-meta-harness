"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def gcd(a: int, b: int) -> int:
    a, b = abs(a), abs(b)
    while b:
        a, b = b, a % b
    return a
