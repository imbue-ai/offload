"""Pytest configuration for example tests."""

import pytest


def pytest_configure(config):
    """Register custom markers."""
    config.addinivalue_line("markers", "group_a: Tests in group A")
    config.addinivalue_line("markers", "group_b: Tests in group B")


def pytest_collection_modifyitems(config, items):
    """Set JUnit XML name to full nodeid at collection time.

    Runs before any test execution, so it works for all tests including
    skipped ones (where the record_xml_attribute fixture would not run).

    Offload relies on matching JUnit test IDs to collected test IDs. When
    the JUnit ``name`` attribute contains ``::`` Offload uses it verbatim,
    bypassing the lossy classname reconstruction.
    """
    xml = None
    for plugin in config.pluginmanager.get_plugins():
        if hasattr(plugin, "node_reporter"):
            xml = plugin
            break
    if xml is None:
        return
    for item in items:
        xml.node_reporter(item.nodeid).add_attribute("name", item.nodeid)
