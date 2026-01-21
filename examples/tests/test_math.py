"""Simple math tests."""

import time


def test_addition():
    time.sleep(2)
    assert 1 + 1 == 2


def test_subtraction():
    time.sleep(2)
    assert 5 - 3 == 2


def test_multiplication():
    time.sleep(2)
    assert 3 * 4 == 12


def test_division():
    time.sleep(2)
    assert 10 / 2 == 5
