"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def word_count(text: str) -> dict:
    out: dict = {}
    for w in text.lower().split():
        out[w] = out.get(w, 0) + 1
    return out
