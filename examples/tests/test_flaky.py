"""Flaky test that fails 50% of the time."""

import random
import pytest


def test_flaky():
    """This test fails on 50% of the runs."""
    if random.random() < 0.5:
        pytest.fail()
