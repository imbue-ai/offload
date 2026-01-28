//! pytest test framework implementation.
//!
//! This module provides test framework support for Python projects using pytest.
//! It uses `pytest --collect-only -q` for test discovery and parses JUnit XML
//! or stdout for results.
//!
//! # Discovery Process
//!
//! 1. Run `pytest --collect-only -q` to list all tests
//! 2. Parse output to extract test IDs (e.g., `tests/test_math.py::test_add`)
//! 3. Generate run commands with `pytest -v --junitxml=/tmp/junit.xml`
//! 4. Parse results from JUnit XML or stdout
//!
//! # Test ID Format
//!
//! pytest test IDs follow the format:
//! ```text
//! path/to/test_file.py::TestClass::test_method
//! path/to/test_file.py::test_function
//! ```
//!
//! # Markers
//!
//! pytest markers are extracted and stored in [`TestCase::markers`].
//! The `markers` configuration option filters tests during discovery:
//!
//! ```toml
//! [groups.python.framework]
//! type = "pytest"
//! markers = "not slow and not integration"
//! ```

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestOutcome, TestRecord,
    TestResult,
};
use crate::config::PytestFrameworkConfig;
use crate::provider::{Command, ExecResult};

/// Test framework for Python pytest projects.
///
/// Uses `pytest --collect-only -q` for test discovery and generates
/// commands with JUnit XML output for structured result parsing.
///
/// # Configuration
///
/// See [`PytestFrameworkConfig`] for available options including:
/// - `paths`: Directories to search
/// - `markers`: Filter expression (e.g., `"not slow"`)
/// - `python`: Python interpreter path
/// - `extra_args`: Additional pytest arguments
pub struct PytestFramework {
    config: PytestFrameworkConfig,
}

impl PytestFramework {
    /// Creates a new pytest framework with the given configuration.
    pub fn new(config: PytestFrameworkConfig) -> Self {
        Self { config }
    }

    /// Parse `pytest --collect-only -q` output to extract test records.
    fn parse_collect_output(&self, output: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();

        for line in output.lines() {
            let trimmed = line.trim();
            // Simple format: tests/test_foo.py::test_bar
            if trimmed.contains("::") && !trimmed.starts_with('<') && !trimmed.contains(' ') {
                tests.push(TestRecord::new(trimmed));
            }
        }

        tests
    }
}

#[async_trait]
impl TestFramework for PytestFramework {
    async fn discover(&self, paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
        // Build the pytest --collect-only command
        let mut cmd = tokio::process::Command::new(&self.config.python);

        cmd.arg("-m").arg("pytest").arg("--collect-only").arg("-q");

        // Add marker filter if specified
        if let Some(markers) = &self.config.markers {
            cmd.arg("-m").arg(markers);
        }

        // Add extra args
        for arg in &self.config.extra_args {
            cmd.arg(arg);
        }

        // Add paths to search
        let search_paths: Vec<_> = if paths.is_empty() {
            self.config
                .paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        } else {
            paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        };

        for path in &search_paths {
            cmd.arg(path);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && !stdout.contains("::") {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "pytest discovery failed: {}",
                stderr
            )));
        }

        let tests = self.parse_collect_output(&stdout);

        if tests.is_empty() {
            tracing::warn!(
                "No tests discovered. stdout: {}, stderr: {}",
                stdout,
                stderr
            );
        }

        Ok(tests)
    }

    fn produce_test_execution_command(&self, tests: &[TestInstance]) -> Command {
        let mut cmd = Command::new(&self.config.python)
            .arg("-m")
            .arg("pytest")
            .arg("-v")
            .arg("--tb=short")
            .arg("--junitxml=/tmp/junit.xml");

        // Add marker filter if specified
        if let Some(markers) = &self.config.markers {
            cmd = cmd.arg("-m").arg(markers);
        }

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(test.id());
        }

        cmd
    }

    fn parse_results(
        &self,
        output: &ExecResult,
        result_file: Option<&str>,
    ) -> FrameworkResult<Vec<TestResult>> {
        let mut results = Vec::new();

        // Try to parse JUnit XML if available
        if let Some(xml_content) = result_file {
            results = parse_junit_xml(xml_content)?;
        }

        // If no JUnit results, parse from stdout
        if results.is_empty() {
            results = parse_pytest_stdout(&output.stdout, &output.stderr)?;
        }

        Ok(results)
    }
}

/// Parse JUnit XML content to extract test results.
fn parse_junit_xml(content: &str) -> FrameworkResult<Vec<TestResult>> {
    // Basic JUnit XML parsing
    // In production, we'd use quick-xml for proper parsing
    let mut results = Vec::new();

    // Simple regex-based parsing for now
    let testcase_re = Regex::new(
        r#"<testcase[^>]*name="([^"]+)"[^>]*classname="([^"]+)"[^>]*time="([^"]+)"[^>]*>"#,
    )
    .unwrap();

    let failure_re = Regex::new(r#"<failure[^>]*message="([^"]*)"[^>]*>"#).unwrap();
    let error_re = Regex::new(r#"<error[^>]*message="([^"]*)"[^>]*>"#).unwrap();
    let skipped_re = Regex::new(r#"<skipped"#).unwrap();

    for cap in testcase_re.captures_iter(content) {
        let name = &cap[1];
        let classname = &cap[2];
        let time: f64 = cap[3].parse().unwrap_or(0.0);

        let test_id = format!("{}::{}", classname.replace('.', "/"), name);

        // Find the content between this testcase and the next (or </testcase>)
        let start = cap.get(0).unwrap().end();
        let end = content[start..]
            .find("</testcase>")
            .map(|i| start + i)
            .unwrap_or(content.len());
        let testcase_content = &content[start..end];

        let (outcome, error_message) = if let Some(fail_cap) = failure_re.captures(testcase_content)
        {
            (TestOutcome::Failed, Some(fail_cap[1].to_string()))
        } else if let Some(err_cap) = error_re.captures(testcase_content) {
            (TestOutcome::Error, Some(err_cap[1].to_string()))
        } else if skipped_re.is_match(testcase_content) {
            (TestOutcome::Skipped, None)
        } else {
            (TestOutcome::Passed, None)
        };

        results.push(TestResult {
            test_id,
            outcome,
            duration: std::time::Duration::from_secs_f64(time),
            stdout: String::new(),
            stderr: String::new(),
            error_message,
            stack_trace: None,
        });
    }

    Ok(results)
}

/// Parse pytest stdout to extract test results.
fn parse_pytest_stdout(stdout: &str, _stderr: &str) -> FrameworkResult<Vec<TestResult>> {
    let mut results = Vec::new();

    // Match lines like:
    // tests/test_foo.py::test_bar PASSED
    // tests/test_foo.py::test_baz FAILED
    let result_re = Regex::new(r"(\S+::\S+)\s+(PASSED|FAILED|SKIPPED|ERROR)").unwrap();

    for cap in result_re.captures_iter(stdout) {
        let test_id = &cap[1];
        let status = &cap[2];

        let outcome = match status {
            "PASSED" => TestOutcome::Passed,
            "FAILED" => TestOutcome::Failed,
            "SKIPPED" => TestOutcome::Skipped,
            "ERROR" => TestOutcome::Error,
            _ => continue,
        };

        results.push(TestResult {
            test_id: test_id.to_string(),
            outcome,
            duration: std::time::Duration::ZERO,
            stdout: String::new(),
            stderr: String::new(),
            error_message: None,
            stack_trace: None,
        });
    }

    Ok(results)
}
