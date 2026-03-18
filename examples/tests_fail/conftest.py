"""Pytest configuration for all-failing test suite."""

import pytest


@pytest.fixture(autouse=True)
def _offload_junit_nodeid(record_xml_attribute, request):
    """Override JUnit name to use the full nodeid, matching pytest --collect-only output.

    By default, pytest converts nodeids like ``tests/test_math.py::TestClass::test_add``
    into JUnit classname=``tests.test_math.TestClass`` and name=``test_add``.  The
    dot-separated classname cannot be losslessly converted back to the original nodeid
    (the ``.py`` extension is stripped and ``::`` separators become dots).

    Offload relies on matching JUnit test IDs to collected test IDs.  When the JUnit
    ``name`` attribute contains ``::`` offload uses it verbatim, bypassing the lossy
    classname reconstruction.  This fixture writes the full nodeid into ``name`` so
    the IDs always match.

    Requires ``junit_family = "xunit1"`` in the project's pytest configuration
    (e.g. ``pyproject.toml`` or ``pytest.ini``).  The ``record_xml_attribute``
    fixture is incompatible with the default ``xunit2`` family.
    """
    record_xml_attribute("name", request.node.nodeid)
