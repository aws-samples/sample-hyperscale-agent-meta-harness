"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def mean(values) -> float:
    values = list(values)
    if not values:
        raise ValueError("empty")
    return sum(values) / len(values)
