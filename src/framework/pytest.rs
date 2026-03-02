//! Pytest framework implementation using `pytest --collect-only` for discovery.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord};
use crate::config::PytestFrameworkConfig;
use crate::provider::Command;

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
    async fn discover(&self, paths: &[PathBuf], filters: &str) -> FrameworkResult<Vec<TestRecord>> {
        // Build the pytest --collect-only command
        let mut cmd = tokio::process::Command::new(&self.config.python);

        // Add extra args
        for arg in &self.config.extra_args {
            cmd.arg(arg);
        }

        cmd.arg("-m").arg("pytest").arg("--collect-only").arg("-q");

        // Add filters if provided, otherwise fall back to markers config
        if !filters.is_empty() {
            let args = shell_words::split(filters).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid filter string '{}': {}",
                    filters, e
                ))
            })?;
            for arg in args {
                cmd.arg(arg);
            }
        } else if let Some(markers) = &self.config.markers {
            cmd.arg("-m").arg(markers);
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

    fn produce_test_execution_command(&self, tests: &[TestInstance], result_path: &str) -> Command {
        let mut cmd = Command::new(&self.config.python);

        for arg in &self.config.extra_args {
            cmd = cmd.arg(arg);
        }

        cmd = cmd
            .arg("-m")
            .arg("pytest")
            .arg("-v")
            .arg("--tb=short")
            .arg(format!("--junitxml={}", result_path));

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(test.id());
        }

        cmd
    }
}
