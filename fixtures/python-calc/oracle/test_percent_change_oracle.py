"""Oracle tests for python-calc. Owned by the E2E harness: they never enter a bundle, a
prompt or the hidden-test set. They check exactly the README's task semantics."""
import pytest

from calc import percent_change


def test_increase() -> None:
    assert percent_change(100, 110) == 10.0


def test_decrease() -> None:
    assert percent_change(50, 25) == -50.0


def test_from_zero_raises() -> None:
    with pytest.raises(ValueError):
        percent_change(0, 5)
