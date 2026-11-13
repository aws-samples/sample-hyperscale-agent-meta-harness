import pytest

from calc import add, ratio, subtract


def test_add() -> None:
    assert add(2, 3) == 5


def test_subtract() -> None:
    assert subtract(10, 4) == 6


def test_ratio() -> None:
    assert ratio(9, 3) == 3.0


def test_ratio_by_zero_raises() -> None:
    with pytest.raises(ValueError):
        ratio(1, 0)
