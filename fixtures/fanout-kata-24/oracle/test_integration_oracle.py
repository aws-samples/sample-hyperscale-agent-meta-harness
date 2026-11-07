"""Oracle tests for fanout-kata-24 R25, `kata.integration`: the integration module
builds on all 24 helpers, so its task runs after they merge. Owned by the E2E harness."""
from kata.integration import NAMES, call

EXPECTED_NAMES = (
    "acronym",
    "chunk",
    "clamp",
    "count_vowels",
    "dedupe",
    "digit_sum",
    "distance",
    "encode",
    "fib",
    "fizzbuzz",
    "flatten",
    "gcd",
    "is_anagram",
    "is_leap",
    "is_palindrome",
    "mean",
    "median",
    "primes_up_to",
    "shift",
    "slugify",
    "to_binary",
    "to_roman",
    "to_snake",
    "word_count",
)


def test_names_list_every_helper_sorted() -> None:
    assert tuple(NAMES) == EXPECTED_NAMES


def test_call_dispatches_to_every_helper() -> None:
    for name, args, want in [
        ("acronym", ("Portable Network Graphics",), "PNG"),
        ("chunk", ([1, 2, 3], 2), [[1, 2], [3]]),
        ("clamp", (5, 0, 3), 3),
        ("count_vowels", ("Programming is fun",), 5),
        ("dedupe", ([3, 1, 3],), [3, 1]),
        ("digit_sum", (-505,), 10),
        ("distance", ("karolin", "kathrin"), 3),
        ("encode", ("aaabcc",), "a3b1c2"),
        ("fib", (10,), 55),
        ("fizzbuzz", (3,), ["1", "2", "Fizz"]),
        ("flatten", ([1, [2, [3]]],), [1, 2, 3]),
        ("gcd", (12, 18), 6),
        ("is_anagram", ("Dormitory", "Dirty room"), True),
        ("is_leap", (2000,), True),
        ("is_palindrome", ("A man, a plan, a canal: Panama",), True),
        ("mean", ([1, 2, 3, 4],), 2.5),
        ("median", ([4, 1, 3, 2],), 2.5),
        ("primes_up_to", (10,), [2, 3, 5, 7]),
        ("shift", ("abc", 1), "bcd"),
        ("slugify", ("Hello, World!",), "hello-world"),
        ("to_binary", (10,), "1010"),
        ("to_roman", (1994,), "MCMXCIV"),
        ("to_snake", ("helloWorld",), "hello_world"),
        ("word_count", ("The cat and the hat",), {"the": 2, "cat": 1, "and": 1, "hat": 1}),
    ]:
        assert call(name, *args) == want, name


def test_an_unknown_name_raises_key_error() -> None:
    try:
        call("nope")
    except KeyError:
        return
    raise AssertionError("call(\"nope\") must raise KeyError")
