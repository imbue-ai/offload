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
/// - `command`: Full pytest invocation command
/// - `run_args`: Extra arguments for execution only
pub struct PytestFramework {
    config: PytestFrameworkConfig,
}

impl PytestFramework {
    /// Creates a new pytest framework with the given configuration.
    pub fn new(config: PytestFrameworkConfig) -> Self {
        Self { config }
    }

    /// Returns (program, args) for the pytest invocation by shell-splitting `command`.
    fn command_prefix(&self) -> (String, Vec<String>) {
        match shell_words::split(&self.config.command) {
            Ok(mut parts) if !parts.is_empty() => {
                let program = parts.remove(0);
                (program, parts)
            }
            _ => {
                tracing::error!(
                    "Failed to parse command '{}'; using 'python -m pytest'",
                    self.config.command
                );
                (
                    "python".to_string(),
                    vec!["-m".to_string(), "pytest".to_string()],
                )
            }
        }
    }

    /// Parse `pytest --collect-only -q` output to extract test records.
    fn parse_collect_output(&self, output: &str, group: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();

        for line in output.lines() {
            let trimmed = line.trim();
            // Simple format: tests/test_foo.py::test_bar
            if trimmed.contains("::") && !trimmed.starts_with('<') && !trimmed.contains(' ') {
                tests.push(TestRecord::new(trimmed, group));
            }
        }

        tests
    }
}

#[async_trait]
impl TestFramework for PytestFramework {
    async fn discover(
        &self,
        paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Build the pytest --collect-only command
        let (program, prefix_args) = self.command_prefix();
        let mut cmd = tokio::process::Command::new(&program);
        for arg in &prefix_args {
            cmd.arg(arg);
        }
        cmd.arg("--collect-only").arg("-q");

        // Add filters if provided
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

        let tests = self.parse_collect_output(&stdout, group);

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
        let (program, prefix_args) = self.command_prefix();
        let mut cmd = Command::new(&program);
        for arg in &prefix_args {
            cmd = cmd.arg(arg);
        }

        cmd = cmd
            .arg("-v")
            .arg("--tb=short")
            .arg(format!("--junitxml={}", result_path));

        // Append run_args for test execution only (not discovery)
        if let Some(run_args) = &self.config.run_args {
            match shell_words::split(run_args) {
                Ok(args) => {
                    for arg in args {
                        cmd = cmd.arg(arg);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse run_args '{}': {}", run_args, e);
                }
            }
        }

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(test.id());
        }

        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PytestFrameworkConfig;

    #[test]
    fn test_command_prefix_with_command() {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config);
        let (program, args) = fw.command_prefix();
        assert_eq!(program, "uv");
        assert_eq!(args, vec!["run", "pytest"]);
    }

    #[test]
    fn test_command_prefix_default() {
        let config = PytestFrameworkConfig::default();
        let fw = PytestFramework::new(config);
        let (program, args) = fw.command_prefix();
        assert_eq!(program, "python");
        assert_eq!(args, vec!["-m", "pytest"]);
    }

    #[test]
    fn test_execution_command_with_run_args() {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            run_args: Some("--no-cov --timeout=30".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config);
        let record = TestRecord::new("tests/test_a.py::test_one", "test-group");
        let tests = vec![TestInstance::new(&record)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml");
        assert_eq!(cmd.program, "uv");
        assert!(cmd.args.contains(&"--no-cov".to_string()));
        assert!(cmd.args.contains(&"--timeout=30".to_string()));
        assert!(cmd.args.contains(&"tests/test_a.py::test_one".to_string()));
    }
}
