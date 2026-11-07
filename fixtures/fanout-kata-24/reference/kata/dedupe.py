"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def dedupe(items) -> list:
    seen = []
    for x in items:
        if x not in seen:
            seen.append(x)
    return seen
