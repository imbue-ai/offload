"""Pytest configuration for example tests."""

import os

import pytest


def pytest_configure(config):
    """Register custom markers."""
    config.addinivalue_line("markers", "group_a: Tests in group A")
    config.addinivalue_line("markers", "group_b: Tests in group B")


def pytest_collection_modifyitems(config, items):
    """Set JUnit XML name to full test ID at collection time.

    Runs before any test execution, so it works for all tests including
    skipped ones (where the record_xml_attribute fixture would not run).

    Uses OFFLOAD_ROOT env var if set (for consistent paths in Offload runs),
    otherwise falls back to pytest's nodeid directly.
    """
    xml = None
    for plugin in config.pluginmanager.get_plugins():
        if hasattr(plugin, "node_reporter"):
            xml = plugin
            break
    if xml is None:
        return
    offload_root = os.environ.get("OFFLOAD_ROOT")
    for item in items:
        if offload_root:
            rel_path = os.path.relpath(str(item.fspath), offload_root)
            parts = item.nodeid.split("::")
            test_id = "::".join([rel_path] + parts[1:])
        else:
            test_id = item.nodeid
        xml.node_reporter(item.nodeid).add_attribute("name", test_id)
