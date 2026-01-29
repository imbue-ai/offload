//! Cargo test framework implementation.
//!
//! This module provides test framework support for Rust projects using `cargo test`.
//! It uses `cargo test --list` for test discovery and parses stdout for results.
//!
//! # Discovery Process
//!
//! 1. Run `cargo test [options] -- --list` to enumerate tests
//! 2. Parse output lines ending in `: test` or `: benchmark`
//! 3. Generate run commands with `cargo test -- --exact <test_names>`
//! 4. Parse results from cargo test stdout
//!
//! # Test ID Format
//!
//! Cargo test IDs follow the Rust module path format:
//! ```text
//! module::submodule::test_function
//! tests::integration::test_scenario
//! ```
//!
//! # Workspace Support
//!
//! For workspaces, specify the package to test:
//!
//! ```toml
//! [groups.rust]
//! type = "cargo"
//! package = "my-crate"
//! features = ["test-utils"]
//! ```
//!
//! # Example Usage
//!
//! ```no_run
//! use offload::framework::cargo::CargoFramework;
//! use offload::framework::TestFramework;
//! use offload::config::CargoFrameworkConfig;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = CargoFrameworkConfig {
//!         package: Some("my-crate".into()),
//!         features: vec!["test-utils".into()],
//!         ..Default::default()
//!     };
//!
//!     let framework = CargoFramework::new(config);
//!     let tests = framework.discover(&[]).await?;
//!
//!     println!("Found {} tests", tests.len());
//!     Ok(())
//! }
//! ```

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestOutcome, TestRecord,
    TestResult,
};
use crate::config::CargoFrameworkConfig;
use crate::provider::{Command, ExecResult};

/// Test framework for Rust projects using `cargo test`.
///
/// Uses `cargo test --list` for test discovery and generates commands
/// with `--exact` flag to run specific tests.
///
/// # Configuration
///
/// See [`CargoFrameworkConfig`] for available options including:
/// - `package`: Package to test (for workspaces)
/// - `features`: Cargo features to enable
/// - `bin`: Binary target name
/// - `include_ignored`: Include `#[ignore]` tests
pub struct CargoFramework {
    config: CargoFrameworkConfig,
}

impl CargoFramework {
    /// Creates a new cargo test framework with the given configuration.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::framework::cargo::CargoFramework;
    /// use offload::config::CargoFrameworkConfig;
    ///
    /// let framework = CargoFramework::new(CargoFrameworkConfig {
    ///     package: Some("my-lib".into()),
    ///     features: vec!["test-utils".into()],
    ///     ..Default::default()
    /// });
    /// ```
    pub fn new(config: CargoFrameworkConfig) -> Self {
        Self { config }
    }

    /// Parse cargo test --list output to extract test records.
    fn parse_list_output(&self, output: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();
        let mut has_doc_tests = false;

        for line in output.lines() {
            let trimmed = line.trim();

            // Skip empty lines and summary lines
            if trimmed.is_empty()
                || trimmed.ends_with("tests")
                    && trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                continue;
            }

            // Test lines end with ": test" or ": benchmark"
            if trimmed.ends_with(": test") {
                let test_name = trimmed.trim_end_matches(": test");
                // Doc tests have names like "src/foo.rs - module (line N)"
                // They can't be filtered with --exact, so we group them
                if test_name.contains(" - ") && test_name.contains("(line") {
                    has_doc_tests = true;
                    continue;
                }
                tests.push(TestRecord::new(test_name));
            } else if trimmed.ends_with(": benchmark") {
                let test_name = trimmed.trim_end_matches(": benchmark");
                tests.push(TestRecord::new(test_name).with_marker("benchmark"));
            }
        }

        // Add a single grouped doc test record if any doc tests exist
        if has_doc_tests {
            tests.push(TestRecord::new("__doctest__").with_marker("doctest"));
        }

        tests
    }
}

#[async_trait]
impl TestFramework for CargoFramework {
    async fn discover(&self, _paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
        // Build the cargo test --list command
        let mut cmd_args = vec!["test".to_string()];

        // Add package if specified
        if let Some(package) = &self.config.package {
            cmd_args.push("-p".to_string());
            cmd_args.push(package.clone());
        }

        // Add features if specified
        if !self.config.features.is_empty() {
            cmd_args.push("--features".to_string());
            cmd_args.push(self.config.features.join(","));
        }

        // Add binary if specified
        if let Some(bin) = &self.config.bin {
            cmd_args.push("--bin".to_string());
            cmd_args.push(bin.clone());
        }

        // Include ignored tests if requested
        if self.config.include_ignored {
            cmd_args.push("--ignored".to_string());
        }

        cmd_args.push("--".to_string());
        cmd_args.push("--list".to_string());

        let output = tokio::process::Command::new("cargo")
            .args(&cmd_args)
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "cargo test --list failed: {}",
                stderr
            )));
        }

        let tests = self.parse_list_output(&stdout);

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
        let mut cmd = Command::new("cargo").arg("test");

        // Add package if specified
        if let Some(package) = &self.config.package {
            cmd = cmd.arg("-p").arg(package);
        }

        // Add features if specified
        if !self.config.features.is_empty() {
            cmd = cmd.arg("--features").arg(self.config.features.join(","));
        }

        // Add binary if specified
        if let Some(bin) = &self.config.bin {
            cmd = cmd.arg("--bin").arg(bin);
        }

        // Include ignored tests if requested
        if self.config.include_ignored {
            cmd = cmd.arg("--ignored");
        }

        // Check if this is a doc test run
        let is_doctest = tests.len() == 1 && tests[0].id() == "__doctest__";

        if is_doctest {
            // Run all doc tests with --doc
            cmd = cmd.arg("--doc");
        } else {
            cmd = cmd.arg("--").arg("--exact");

            // Add test names
            for test in tests {
                cmd = cmd.arg(test.id());
            }
        }

        cmd
    }

    fn parse_results(
        &self,
        output: &ExecResult,
        _result_file: Option<&str>,
    ) -> FrameworkResult<Vec<TestResult>> {
        parse_cargo_test_output(&output.stdout, &output.stderr)
    }
}

/// Parse cargo test output to extract test results.
fn parse_cargo_test_output(stdout: &str, _stderr: &str) -> FrameworkResult<Vec<TestResult>> {
    let mut results = Vec::new();

    // Match lines like:
    // test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
    // test tests::test_foo ... ok
    // test tests::test_bar ... FAILED
    let result_re = Regex::new(r"test\s+(\S+)\s+\.\.\.\s+(ok|FAILED|ignored)").unwrap();

    for cap in result_re.captures_iter(stdout) {
        let test_id = &cap[1];
        let status = &cap[2];

        let outcome = match status {
            "ok" => TestOutcome::Passed,
            "FAILED" => TestOutcome::Failed,
            "ignored" => TestOutcome::Skipped,
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

    // Handle grouped doc tests - if results is empty but we have a passing summary,
    // this is likely a doc test run (individual doc test names have spaces and don't match \S+)
    // Look for summary line: "test result: ok. N passed; M failed; ..."
    if results.is_empty() {
        let summary_re = Regex::new(r"test result: ok\. (\d+) passed; (\d+) failed;").unwrap();
        if let Some(cap) = summary_re.captures(stdout) {
            let passed: u32 = cap[1].parse().unwrap_or(0);
            let failed: u32 = cap[2].parse().unwrap_or(0);

            if passed > 0 || failed > 0 {
                let outcome = if failed == 0 {
                    TestOutcome::Passed
                } else {
                    TestOutcome::Failed
                };

                results.push(TestResult {
                    test_id: "__doctest__".to_string(),
                    outcome,
                    duration: std::time::Duration::ZERO,
                    stdout: format!("{} doc tests passed, {} failed", passed, failed),
                    stderr: String::new(),
                    error_message: if failed > 0 {
                        Some(format!("{} doc tests failed", failed))
                    } else {
                        None
                    },
                    stack_trace: None,
                });
            }
        }
    }

    // Try to extract failure details by splitting on test output sections
    let section_re = Regex::new(r"---- (\S+) stdout ----\n").unwrap();
    let sections: Vec<_> = section_re.split(stdout).collect();
    let test_ids: Vec<_> = section_re
        .captures_iter(stdout)
        .map(|c| c[1].to_string())
        .collect();

    // sections[0] is before first match, sections[i+1] is content after test_ids[i]
    for (i, test_id) in test_ids.iter().enumerate() {
        if let Some(content) = sections.get(i + 1) {
            // Content ends at next section or "failures:" marker
            let output_content = content.split("\n\nfailures:").next().unwrap_or(content);

            // Find and update the corresponding result
            for result in &mut results {
                if result.test_id == *test_id {
                    result.stdout = output_content.to_string();
                    if result.outcome == TestOutcome::Failed {
                        result.error_message =
                            Some(output_content.lines().last().unwrap_or("").to_string());
                    }
                    break;
                }
            }
        }
    }

    Ok(results)
}
