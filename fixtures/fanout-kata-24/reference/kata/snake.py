"""Reference solution for the oracle self-test; never staged in a fixture repository."""

import re


def to_snake(name: str) -> str:
    return re.sub(r"(?<=[a-z0-9])([A-Z])", r"_\1", name).lower()
