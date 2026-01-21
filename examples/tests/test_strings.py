"""Simple string tests."""


def test_concatenation():
    assert "hello" + " " + "world" == "hello world"


def test_upper():
    assert "hello".upper() == "HELLO"


def test_split():
    assert "a,b,c".split(",") == ["a", "b", "c"]


def test_strip():
    assert "  spaced  ".strip() == "spaced"
