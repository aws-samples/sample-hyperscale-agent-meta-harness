"""Reference solution for the oracle self-test; never staged in a fixture repository."""
from kata.acronym import acronym
from kata.anagram import is_anagram
from kata.binary import to_binary
from kata.caesar import shift
from kata.dedupe import dedupe
from kata.digits import digit_sum
from kata.fibonacci import fib
from kata.fizzbuzz import fizzbuzz
from kata.flatten import flatten
from kata.gcd import gcd
from kata.hamming import distance
from kata.leap import is_leap
from kata.lists import chunk
from kata.mean import mean
from kata.median import median
from kata.numbers import clamp
from kata.palindrome import is_palindrome
from kata.primes import primes_up_to
from kata.rle import encode
from kata.roman import to_roman
from kata.snake import to_snake
from kata.strings import slugify
from kata.text import word_count
from kata.vowels import count_vowels

_HELPERS = {f.__name__: f for f in (acronym, chunk, clamp, count_vowels, dedupe, digit_sum, distance, encode, fib, fizzbuzz, flatten, gcd, is_anagram, is_leap, is_palindrome, mean, median, primes_up_to, shift, slugify, to_binary, to_roman, to_snake, word_count)}  # nosemgrep: is-function-without-parentheses -- the function is listed or passed as an object on purpose, not called there
NAMES = tuple(sorted(_HELPERS))


def call(name, *args):
    if name not in _HELPERS:
        raise KeyError(name)
    return _HELPERS[name](*args)
