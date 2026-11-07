"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def encode(text: str) -> str:
    out = []
    i = 0
    while i < len(text):
        j = i
        while j < len(text) and text[j] == text[i]:
            j += 1
        out.append(f"{text[i]}{j - i}")
        i = j
    return "".join(out)
