"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def primes_up_to(n: int) -> list:
    return [p for p in range(2, n + 1) if all(p % d for d in range(2, int(p ** 0.5) + 1))]
