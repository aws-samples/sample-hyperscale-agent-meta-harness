"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def is_anagram(a: str, b: str) -> bool:
    def key(s: str) -> list:
        return sorted(c.lower() for c in s if c.isalpha())
    return key(a) == key(b)
