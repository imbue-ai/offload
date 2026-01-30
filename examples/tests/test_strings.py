"""Simple string tests."""

import pytest


@pytest.mark.group_a
def test_concatenation():
    assert "hello" + " " + "world" == "hello world"


@pytest.mark.group_a
def test_upper():
    assert "hello".upper() == "HELLO"


@pytest.mark.group_b
def test_split():
    assert "a,b,c".split(",") == ["a", "b", "c"]


@pytest.mark.group_b
def test_strip():
    assert "  spaced  ".strip() == "spaced"
