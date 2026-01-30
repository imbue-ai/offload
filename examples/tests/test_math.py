"""Simple math tests."""

import time
import pytest


@pytest.mark.group_a
def test_addition():
    time.sleep(2)
    assert 1 + 1 == 2


@pytest.mark.group_a
def test_subtraction():
    time.sleep(2)
    assert 5 - 3 == 2


@pytest.mark.group_b
def test_multiplication():
    time.sleep(2)
    assert 3 * 4 == 12


@pytest.mark.group_b
def test_division():
    time.sleep(2)
    assert 10 / 2 == 5
