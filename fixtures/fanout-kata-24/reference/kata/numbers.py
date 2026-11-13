"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def clamp(value, low, high):
    if low > high:
        raise ValueError("low > high")
    return max(low, min(high, value))
