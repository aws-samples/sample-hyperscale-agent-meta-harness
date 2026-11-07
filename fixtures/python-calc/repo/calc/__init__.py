"""A small calculator library."""


def add(a: float, b: float) -> float:
    """Return a + b."""
    return a + b


def subtract(a: float, b: float) -> float:
    """Return a - b."""
    return a - b


def ratio(a: float, b: float) -> float:
    """Return a / b; b must not be zero."""
    if b == 0:
        raise ValueError("ratio with a zero denominator is undefined")
    return a / b
