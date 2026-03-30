"""Tests that produce artifact files for download_globs testing."""

import json
import os

ARTIFACT_DIR = os.path.join(os.getcwd(), "artifact-output")


def _ensure_dir(path):
    os.makedirs(path, exist_ok=True)


def test_produces_json_report():
    """Writes a JSON summary report."""
    _ensure_dir(ARTIFACT_DIR)
    report = {
        "test": "test_produces_json_report",
        "status": "passed",
        "metrics": {"duration_ms": 42, "memory_mb": 128},
    }
    path = os.path.join(ARTIFACT_DIR, "report.json")
    with open(path, "w") as f:
        json.dump(report, f, indent=2)
    assert os.path.exists(path)


def test_produces_log_file():
    """Writes a log file."""
    _ensure_dir(ARTIFACT_DIR)
    path = os.path.join(ARTIFACT_DIR, "test.log")
    with open(path, "w") as f:
        for i in range(20):
            f.write(f"[INFO] Step {i}: processing item batch\n")
    assert os.path.exists(path)


def test_produces_csv_data():
    """Writes a CSV data file."""
    subdir = os.path.join(ARTIFACT_DIR, "data")
    _ensure_dir(subdir)
    path = os.path.join(subdir, "results.csv")
    with open(path, "w") as f:
        f.write("test_name,passed,duration_ms\n")
        f.write("test_a,true,10\n")
        f.write("test_b,true,25\n")
        f.write("test_c,false,100\n")
    assert os.path.exists(path)


def test_produces_nested_xml():
    """Writes an XML file in a nested directory."""
    subdir = os.path.join(ARTIFACT_DIR, "reports", "coverage")
    _ensure_dir(subdir)
    path = os.path.join(subdir, "coverage.xml")
    with open(path, "w") as f:
        f.write('<?xml version="1.0" ?>\n')
        f.write('<coverage version="7.0" timestamp="1234567890">\n')
        f.write("  <packages>\n")
        f.write('    <package name="mypackage" line-rate="0.85" />\n')
        f.write("  </packages>\n")
        f.write("</coverage>\n")
    assert os.path.exists(path)
