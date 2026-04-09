"""Pytest configuration for all-failing test suite."""

import pytest
from _pytest.junitxml import xml_key


def pytest_collection_modifyitems(config, items):
    """Set JUnit XML name to full nodeid at collection time.

    Runs before any test execution, so it works for all tests including
    skipped ones (where the record_xml_attribute fixture would not run).

    Offload relies on matching JUnit test IDs to collected test IDs. When
    the JUnit ``name`` attribute contains ``::`` Offload uses it verbatim,
    bypassing the lossy classname reconstruction.
    """
    xml = config.stash.get(xml_key, None)
    if xml is None:
        return
    for item in items:
        xml.node_reporter(item.nodeid).add_attribute("name", item.nodeid)
