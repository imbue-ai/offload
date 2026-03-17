"""Tests for verifying --fail-fast behavior.

One test fails immediately. The rest sleep for 10 minutes and pass.
If --fail-fast works, the run finishes in seconds instead of 10 minutes.
"""

import time


def test_fail_immediately():
    assert False, "intentional failure"


def test_slow_pass_1():
    time.sleep(600)


def test_slow_pass_2():
    time.sleep(600)


def test_slow_pass_3():
    time.sleep(600)


def test_slow_pass_4():
    time.sleep(600)


def test_slow_pass_5():
    time.sleep(600)


def test_slow_pass_6():
    time.sleep(600)


def test_slow_pass_7():
    time.sleep(600)


def test_slow_pass_8():
    time.sleep(600)


def test_slow_pass_9():
    time.sleep(600)
