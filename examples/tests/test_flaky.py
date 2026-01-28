"""Flaky test that fails on first attempt but passes on retry."""
import os
import tempfile

MARKER_FILE = os.path.join(tempfile.gettempdir(), "flaky_test_marker")

def test_flaky():
    """This test fails on first run, passes on second."""
    if os.path.exists(MARKER_FILE):
        # Second run - pass and clean up
        os.remove(MARKER_FILE)
        assert True
    else:
        # First run - create marker and fail
        with open(MARKER_FILE, "w") as f:
            f.write("1")
        assert False, "Intentional first failure"
