"""Tests in a subdirectory, used to exercise --ignore of a directory."""


def test_sub_alpha():
    assert 1 + 1 == 2


def test_sub_beta():
    assert "sub".upper() == "SUB"
