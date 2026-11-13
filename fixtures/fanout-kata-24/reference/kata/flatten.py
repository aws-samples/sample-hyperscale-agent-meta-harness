"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def flatten(nested) -> list:
    out = []
    for x in nested:
        if isinstance(x, list):
            out.extend(flatten(x))
        else:
            out.append(x)
    return out
