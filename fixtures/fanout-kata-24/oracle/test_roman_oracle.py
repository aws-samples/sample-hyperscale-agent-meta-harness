"""Oracle tests for fanout-kata-24 R6, `kata.roman.to_roman`. Owned by the E2E harness:
never staged in a bundle, a prompt or the hidden-test set."""
from kata.roman import to_roman


def test_numerals() -> None:
    assert to_roman(1994) == "MCMXCIV"
    assert to_roman(4) == "IV"
    assert to_roman(3999) == "MMMCMXCIX"


def test_out_of_range_raises() -> None:
    try:
        to_roman(0)
    except ValueError:
        return
    raise AssertionError('to_roman(0) must raise ValueError')
