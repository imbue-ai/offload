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
    /// Nextest output formats:
    /// - `binary_name module::test_func` - tests in lib.rs
    /// - `binary::test_file test_func` - tests in integration test files
    ///
    /// Returns test IDs in the format `binary test_name` to match
    /// JUnit XML output where classname=binary and name=test_name.
    fn parse_list_output(&self, output: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();
        let mut current_binary: Option<String> = None;

        for line in output.lines() {
            let trimmed = line.trim();

            // Skip empty lines and build output
            if trimmed.is_empty()
                || trimmed.starts_with("Compiling")
                || trimmed.starts_with("Finished")
            {
                continue;
            }

            // Nextest list has two output formats:
            //
            // Indented format (default):
            //   binary_name:
            //       module::test_func
            //
            // Flat format:
            //   binary_name module::test_func

            if let Some(binary) = trimmed.strip_suffix(':') {
                // "binary_name:" header line
                current_binary = Some(binary.to_string());
            } else {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    // Flat format: "binary test_path"
                    let test_id = format!("{} {}", parts[0], parts[1]);
                    tests.push(TestRecord::new(&test_id));
                } else if parts.len() == 1
                    && let Some(ref binary) = current_binary
                {
                    // Indented format: test name under a binary header
                    let test_id = format!("{} {}", binary, parts[0]);
                    tests.push(TestRecord::new(&test_id));
                }
            }
        }

        tests
    }
}

#[async_trait]
impl TestFramework for CargoFramework {
    async fn discover(&self, _paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
        let mut cmd_args = vec![
            "nextest".to_string(),
            "list".to_string(),
            "--color=never".to_string(), // Prevent ANSI codes in test names
            "-T".to_string(),            // Explicit output format: "binary test_path"
            "oneline".to_string(),       // (default varies by nextest version / TTY)
        ];

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
        } else {
            tracing::info!("Discovered {} tests", tests.len());
        }

        Ok(tests)
    }

    fn produce_test_execution_command(&self, tests: &[TestInstance], result_path: &str) -> Command {
        // Nextest configures JUnit output via config file, not CLI flags.
        // Write a temporary config that sets the JUnit path, then run nextest
        // with --config-file pointing to it. This ensures each sandbox writes
        // to a unique path, avoiding collisions with the local provider.
        let config_path = format!("{}.nextest.toml", result_path);

        let mut args = vec![
            "nextest".to_string(),
            "run".to_string(),
            "--no-fail-fast".to_string(),
            "--config-file".to_string(),
            config_path.clone(),
        ];

        if let Some(package) = &self.config.package {
            args.push("-p".to_string());
            args.push(package.clone());
        }

        if !self.config.features.is_empty() {
            args.push("--features".to_string());
            args.push(self.config.features.join(","));
        }

        if let Some(bin) = &self.config.bin {
            args.push("--bin".to_string());
            args.push(bin.clone());
        }

        if self.config.include_ignored {
            args.push("--run-ignored".to_string());
            args.push("only".to_string());
        }

        // Build filter expression: (binary(=b1) & test(=t1)) | (binary(=b2) & test(=t2)) | ...
        // Test IDs are in format "binary_name test::path", we need both to uniquely identify tests
        let filter_expr: String = tests
            .iter()
            .map(|t| {
                let id = t.id();
                // Split into binary and test path; fall back to just test filter if no space
                if let Some((binary, test_path)) = id.split_once(' ') {
                    format!("(binary(={}) & test(={}))", binary, test_path)
                } else {
                    format!("test(={})", id)
                }
            })
            .collect::<Vec<_>>()
            .join(" | ");

        args.push("-E".to_string());
        args.push(filter_expr);

        let cargo_args = args
            .iter()
            .map(|a| shell_words::quote(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        // Write a nextest config with the unique JUnit path, then run cargo nextest
        let shell_cmd = format!(
            "cat > {config_path} << 'NEXTEST_EOF'\n\
             [profile.default.junit]\n\
             path = \"{result_path}\"\n\
             NEXTEST_EOF\n\
             cargo {cargo_args}",
        );

        Command::new("sh").arg("-c").arg(&shell_cmd)
    }

    fn parse_results(
        &self,
        _output: &ExecResult,
        result_file: Option<&str>,
    ) -> FrameworkResult<Vec<TestResult>> {
        if let Some(xml) = result_file {
            // Cargo nextest always uses "{classname} {name}" format where
            // classname is the binary name and name is the test function path
            parse_junit_xml(xml, "{classname} {name}")
        } else {
            Ok(Vec::new())
        }
    }
}
