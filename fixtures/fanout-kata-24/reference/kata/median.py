"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def median(values):
    s = sorted(values)
    if not s:
        raise ValueError("empty")
    mid = len(s) // 2
    return s[mid] if len(s) % 2 else (s[mid - 1] + s[mid]) / 2
