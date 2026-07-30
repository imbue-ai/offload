"""Parametrized tests whose node IDs stay space-free for discovery parity."""

import pytest


@pytest.mark.parametrize("value", [1, 2, 3])
def test_square_is_non_negative(value):
    assert value * value >= 0
