"""Simple list tests."""


def test_append():
    lst = [1, 2]
    lst.append(3)
    assert lst == [1, 2, 3]


def test_length():
    assert len([1, 2, 3, 4, 5]) == 5


def test_slice():
    assert [1, 2, 3, 4, 5][1:4] == [2, 3, 4]


def test_reverse():
    lst = [1, 2, 3]
    lst.reverse()
    assert lst == [3, 2, 1]
