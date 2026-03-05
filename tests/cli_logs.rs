use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use assert_cmd::Command;
use offload::config::{
    Config, FrameworkConfig, GroupConfig, LocalProviderConfig, OffloadConfig, ProviderConfig,
    PytestFrameworkConfig, ReportConfig,
};
use predicates::prelude::*;
use tempfile::TempDir;

#[allow(deprecated)]
fn offload_cmd() -> anyhow::Result<Command> {
    Command::cargo_bin("offload").context("offload binary not found")
}

/// Create a minimal valid offload.toml pointing to the given output_dir.
fn write_config(config_path: &Path, output_dir: &Path) -> anyhow::Result<()> {
    let config = Config {
        offload: OffloadConfig {
            max_parallel: 1,
            test_timeout_secs: 300,
            working_dir: None,
            stream_output: false,
            sandbox_project_root: ".".to_string(),
            sandbox_init_cmd: None,
        },
        provider: ProviderConfig::Local(LocalProviderConfig::default()),
        framework: FrameworkConfig::Pytest(PytestFrameworkConfig::default()),
        groups: HashMap::from([("all".to_string(), GroupConfig::default())]),
        report: ReportConfig {
            output_dir: PathBuf::from(output_dir),
            junit: true,
            junit_file: "junit.xml".to_string(),
        },
    };
    let content = toml::to_string_pretty(&config).context("failed to serialize config")?;
    fs::write(config_path, content).context("failed to write config")?;
    Ok(())
}

/// Write a JUnit XML file with a mix of passed, failed, and errored tests.
fn write_junit_xml(output_dir: &Path) -> anyhow::Result<()> {
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
    fs::create_dir_all(output_dir).context("failed to create output dir")?;
    fs::write(output_dir.join("junit.xml"), xml).context("failed to write junit.xml")?;
    Ok(())
}

/// Write a JUnit XML file with only passing tests.
fn write_passing_junit_xml(output_dir: &Path) -> anyhow::Result<()> {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="offload" tests="2" failures="0" errors="0" time="1.0">
  <testsuite name="pytest" tests="2" failures="0" errors="0" skipped="0" time="1.0">
    <testcase name="tests/test_math.py::test_add" classname="tests.test_math" time="0.1"/>
    <testcase name="tests/test_math.py::test_sub" classname="tests.test_math" time="0.2"/>
  </testsuite>
</testsuites>"#;
    fs::create_dir_all(output_dir).context("failed to create output dir")?;
    fs::write(output_dir.join("junit.xml"), xml).context("failed to write junit.xml")?;
    Ok(())
}

#[test]
fn test_logs_no_junit_file() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    fs::create_dir_all(&output_dir)?;
    // No junit.xml written

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("No test results found"));
    Ok(())
}

#[test]
fn test_logs_shows_all_results() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
        ])
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
    Ok(())
}

#[test]
fn test_logs_failures_filter() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--failures",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_div"))
        .stdout(predicate::str::contains("FAILED"))
        .stdout(predicate::str::contains("test_add").not())
        .stdout(predicate::str::contains("test_sub").not())
        .stdout(predicate::str::contains("test_connect").not());
    Ok(())
}

#[test]
fn test_logs_errors_filter() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--errors",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_connect"))
        .stdout(predicate::str::contains("ERROR"))
        .stdout(predicate::str::contains("test_add").not())
        .stdout(predicate::str::contains("test_sub").not())
        .stdout(predicate::str::contains("test_div").not());
    Ok(())
}

#[test]
fn test_logs_failures_and_errors() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
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
    Ok(())
}

#[test]
fn test_logs_no_matching_results() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_passing_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--failures",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("No matching test results found"));
    Ok(())
}

#[test]
fn test_logs_test_exact_single() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--test",
            "tests/test_math.py::test_add",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_add"))
        .stdout(predicate::str::contains("test_sub").not())
        .stdout(predicate::str::contains("test_div").not())
        .stdout(predicate::str::contains("test_connect").not());
    Ok(())
}

#[test]
fn test_logs_test_exact_multiple() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--test",
            "tests/test_math.py::test_add",
            "--test",
            "tests/test_math.py::test_div",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_add"))
        .stdout(predicate::str::contains("test_div"))
        .stdout(predicate::str::contains("test_sub").not())
        .stdout(predicate::str::contains("test_connect").not());
    Ok(())
}

#[test]
fn test_logs_test_regex_substring() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    // Matches both test_math tests and test_div (all in test_math.py)
    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--test-regex",
            "test_math",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_add"))
        .stdout(predicate::str::contains("test_sub"))
        .stdout(predicate::str::contains("test_div"))
        .stdout(predicate::str::contains("test_connect").not());
    Ok(())
}

#[test]
fn test_logs_test_with_failures_filter() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    // --test-regex matches all test_math tests, --failures narrows to only the failed one
    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--test-regex",
            "test_math",
            "--failures",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test_div"))
        .stdout(predicate::str::contains("FAILED"))
        .stdout(predicate::str::contains("test_add").not())
        .stdout(predicate::str::contains("test_sub").not());
    Ok(())
}

#[test]
fn test_logs_test_regex_invalid() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    write_junit_xml(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_config(&config_path, &output_dir)?;

    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "logs",
            "--test-regex",
            "[invalid",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid --test-regex pattern"));
    Ok(())
}
