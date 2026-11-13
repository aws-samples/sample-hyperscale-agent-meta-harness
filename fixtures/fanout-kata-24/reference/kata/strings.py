"""Reference solution for the oracle self-test; never staged in a fixture repository."""

import re


def slugify(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
