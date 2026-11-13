"""Reference solution for the oracle self-test; never staged in a fixture repository."""

import re


def acronym(text: str) -> str:
    return "".join(w[0].upper() for w in re.split(r"[\s-]+", text) if w)
