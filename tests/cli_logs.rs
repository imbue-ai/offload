use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[allow(deprecated)]
fn offload_cmd() -> Command {
    Command::cargo_bin("offload").expect("offload binary not found")
}

/// Create a minimal valid offload.toml pointing to the given output_dir.
fn write_config(config_path: &Path, output_dir: &Path) {
    let content = format!(
        r#"[offload]
max_parallel = 1
sandbox_project_root = "."

[provider]
type = "local"

[framework]
type = "pytest"

[groups.all]

[report]
output_dir = "{}"
"#,
        output_dir.display()
    );
    fs::write(config_path, content).expect("failed to write config");
}

/// Write a JUnit XML file with a mix of passed, failed, and errored tests.
fn write_junit_xml(output_dir: &Path) {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="offload" tests="4" failures="1" errors="1" time="5.0">
  <testsuite name="pytest" tests="4" failures="1" errors="1" skipped="0" time="5.0">
    <testcase name="tests/test_math.py::test_add" classname="tests.test_math" time="0.1"/>
    <testcase name="tests/test_math.py::test_sub" classname="tests.test_math" time="0.2"/>
    <testcase name="tests/test_math.py::test_div" classname="tests.test_math" time="0.3">
      <failure message="AssertionError: expected 2 got 3&#10;assert 1 / 0 == 2">tests/test_math.py:10: in test_div
    assert 1 / 0 == 2
E   AssertionError: expected 2 got 3</failure>
    </testcase>
    <testcase name="tests/test_net.py::test_connect" classname="tests.test_net" time="1.0">
      <error message="ConnectionError: refused">tests/test_net.py:5: in test_connect
    socket.connect(...)
E   ConnectionError: refused</error>
    </testcase>
  </testsuite>
</testsuites>"#;
    fs::create_dir_all(output_dir).expect("failed to create output dir");
    fs::write(output_dir.join("junit.xml"), xml).expect("failed to write junit.xml");
}

/// Write a JUnit XML file with only passing tests.
fn write_passing_junit_xml(output_dir: &Path) {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="offload" tests="2" failures="0" errors="0" time="1.0">
  <testsuite name="pytest" tests="2" failures="0" errors="0" skipped="0" time="1.0">
    <testcase name="tests/test_math.py::test_add" classname="tests.test_math" time="0.1"/>
    <testcase name="tests/test_math.py::test_sub" classname="tests.test_math" time="0.2"/>
  </testsuite>
</testsuites>"#;
    fs::create_dir_all(output_dir).expect("failed to create output dir");
    fs::write(output_dir.join("junit.xml"), xml).expect("failed to write junit.xml");
}

#[test]
fn test_logs_no_junit_file() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let output_dir = tmp.path().join("results");
    fs::create_dir_all(&output_dir).expect("failed to create output dir");
    // No junit.xml written

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir);

    offload_cmd()
        .args(["-c", config_path.to_str().unwrap(), "logs"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("No test results found"));
}

#[test]
fn test_logs_shows_all_results() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir);

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir);

    offload_cmd()
        .args(["-c", config_path.to_str().unwrap(), "logs"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "=== tests/test_math.py::test_add [PASSED] ===",
        ))
        .stdout(predicate::str::contains(
            "=== tests/test_math.py::test_sub [PASSED] ===",
        ))
        .stdout(predicate::str::contains(
            "=== tests/test_math.py::test_div [FAILED] ===",
        ))
        .stdout(predicate::str::contains("AssertionError: expected 2 got 3"))
        .stdout(predicate::str::contains(
            "=== tests/test_net.py::test_connect [ERROR] ===",
        ))
        .stdout(predicate::str::contains("ConnectionError: refused"));
}

#[test]
fn test_logs_failures_filter() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir);

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir);

    offload_cmd()
        .args(["-c", config_path.to_str().unwrap(), "logs", "--failures"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_div"))
        .stdout(predicate::str::contains("FAILED"))
        .stdout(predicate::str::contains("test_add").not())
        .stdout(predicate::str::contains("test_sub").not())
        .stdout(predicate::str::contains("test_connect").not());
}

#[test]
fn test_logs_errors_filter() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir);

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir);

    offload_cmd()
        .args(["-c", config_path.to_str().unwrap(), "logs", "--errors"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_connect"))
        .stdout(predicate::str::contains("ERROR"))
        .stdout(predicate::str::contains("test_add").not())
        .stdout(predicate::str::contains("test_sub").not())
        .stdout(predicate::str::contains("test_div").not());
}

#[test]
fn test_logs_failures_and_errors() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir);

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir);

    offload_cmd()
        .args([
            "-c",
            config_path.to_str().unwrap(),
            "logs",
            "--failures",
            "--errors",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_div"))
        .stdout(predicate::str::contains("test_connect"))
        .stdout(predicate::str::contains("test_add").not())
        .stdout(predicate::str::contains("test_sub").not());
}

#[test]
fn test_logs_no_matching_results() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let output_dir = tmp.path().join("results");
    write_passing_junit_xml(&output_dir);

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir);

    offload_cmd()
        .args(["-c", config_path.to_str().unwrap(), "logs", "--failures"])
        .assert()
        .success()
        .stderr(predicate::str::contains("No matching test results found"));
}
