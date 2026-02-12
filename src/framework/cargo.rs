//! Cargo test framework implementation.
//!
//! This module provides test framework support for Rust projects using `cargo nextest`.
//! It uses `cargo nextest list` for test discovery and parses JUnit XML for results.
//!
//! # Discovery Process
//!
//! 1. Run `cargo nextest list` to enumerate tests
//! 2. Parse output lines containing `::` test paths
//! 3. Generate run commands with `cargo nextest run --junit-file`
//! 4. Parse results from JUnit XML
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

use super::{FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord, TestResult};
use crate::config::CargoFrameworkConfig;
use crate::framework::pytest::parse_junit_xml;
use crate::provider::{Command, ExecResult};

/// Test framework for Rust projects using `cargo nextest`.
///
/// Uses `cargo nextest list` for test discovery and generates commands
/// with JUnit XML output for structured result parsing.
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

    /// Parse cargo nextest list output to extract test records.
    ///
    /// Nextest output format: `binary_name test::path::here`
    fn parse_list_output(&self, output: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();

        for line in output.lines() {
            let trimmed = line.trim();

            // Skip empty lines and build output
            if trimmed.is_empty()
                || trimmed.starts_with("Compiling")
                || trimmed.starts_with("Finished")
            {
                continue;
            }

            // Nextest format: "binary_name test::path"
            // Extract the test path (part with ::)
            if let Some(test_path) = trimmed.split_whitespace().find(|s| s.contains("::")) {
                tests.push(TestRecord::new(test_path));
            }
        }

        tests
    }
}

#[async_trait]
impl TestFramework for CargoFramework {
    async fn discover(&self, _paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
        let mut cmd_args = vec!["nextest".to_string(), "list".to_string()];

        if let Some(package) = &self.config.package {
            cmd_args.push("-p".to_string());
            cmd_args.push(package.clone());
        }

        if !self.config.features.is_empty() {
            cmd_args.push("--features".to_string());
            cmd_args.push(self.config.features.join(","));
        }

        if let Some(bin) = &self.config.bin {
            cmd_args.push("--bin".to_string());
            cmd_args.push(bin.clone());
        }

        if self.config.include_ignored {
            cmd_args.push("--run-ignored".to_string());
            cmd_args.push("only".to_string());
        }

        let output = tokio::process::Command::new("cargo")
            .args(&cmd_args)
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "cargo nextest list failed: {}",
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
        // JUnit output configured via .config/nextest.toml
        let mut cmd = Command::new("cargo")
            .arg("nextest")
            .arg("run")
            .arg("--no-fail-fast");

        if let Some(package) = &self.config.package {
            cmd = cmd.arg("-p").arg(package);
        }

        if !self.config.features.is_empty() {
            cmd = cmd.arg("--features").arg(self.config.features.join(","));
        }

        if let Some(bin) = &self.config.bin {
            cmd = cmd.arg("--bin").arg(bin);
        }

        if self.config.include_ignored {
            cmd = cmd.arg("--run-ignored").arg("only");
        }

        // Build filter expression: test(=test1) | test(=test2) | ...
        let filter_expr: String = tests
            .iter()
            .map(|t| format!("test(={})", t.id()))
            .collect::<Vec<_>>()
            .join(" | ");

        cmd.arg("-E").arg(&filter_expr)
    }

    fn parse_results(
        &self,
        _output: &ExecResult,
        result_file: Option<&str>,
    ) -> FrameworkResult<Vec<TestResult>> {
        if let Some(xml) = result_file {
            parse_junit_xml(xml)
        } else {
            Ok(Vec::new())
        }
    }
}
