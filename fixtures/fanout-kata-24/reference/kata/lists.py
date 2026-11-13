"""Reference solution for the oracle self-test; never staged in a fixture repository."""


def chunk(items, size):
    if size < 1:
        raise ValueError("size < 1")
    items = list(items)
    return [items[i:i + size] for i in range(0, len(items), size)]
