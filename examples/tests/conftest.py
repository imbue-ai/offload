"""Pytest configuration for example tests."""

import pytest


def pytest_configure(config):
    """Register custom markers."""
    config.addinivalue_line("markers", "group_a: Tests in group A")
    config.addinivalue_line("markers", "group_b: Tests in group B")
