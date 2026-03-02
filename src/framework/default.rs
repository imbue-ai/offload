//! Default framework — custom shell commands for test discovery and execution.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord};
use crate::config::DefaultFrameworkConfig;
use crate::provider::Command;

/// Test framework using custom shell commands.
///
/// Provides maximum flexibility by delegating test discovery and execution
/// to user-defined shell commands. Suitable for any test framework.
///
/// # Configuration
///
/// See [`DefaultFrameworkConfig`] for available options including:
/// - `discover_command`: Shell command that outputs test IDs
/// - `run_command`: Command template with `{tests}` placeholder
/// - `result_file`: Optional JUnit XML path for detailed results
/// - `working_dir`: Directory for running commands
pub struct DefaultFramework {
    config: DefaultFrameworkConfig,
}

impl DefaultFramework {
    /// Creates a new default framework with the given configuration.
    pub fn new(config: DefaultFrameworkConfig) -> Self {
        Self { config }
    }

    /// Parse test discovery command output to extract test records.
    ///
    /// Expects one test ID per line.
    fn parse_discover_output(&self, output: &str) -> Vec<TestRecord> {
        output
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(TestRecord::new)
            .collect()
    }

    /// Substitute {tests} and {result_file} placeholders in run command.
    ///
    /// Test IDs are shell-escaped to handle IDs containing spaces or special characters.
    fn substitute_command(&self, tests: &[TestInstance], result_path: &str) -> String {
        let test_ids: Vec<_> = tests
            .iter()
            .map(|t| shell_words::quote(t.id()).into_owned())
            .collect();
        self.config
            .run_command
            .replace("{tests}", &test_ids.join(" "))
            .replace("{result_file}", result_path)
    }
}

#[async_trait]
impl TestFramework for DefaultFramework {
    async fn discover(
        &self,
        _paths: &[PathBuf],
        filters: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Substitute {filters} placeholder with actual filters or empty string
        let discover_command = self.config.discover_command.replace("{filters}", filters);

        // Run test discovery command through shell to support pipes, globs, etc.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c");
        cmd.arg(&discover_command);

        if let Some(dir) = &self.config.working_dir {
            cmd.current_dir(dir);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        tracing::debug!("Discovery stdout:\n{}", stdout);
        if !stderr.is_empty() {
            tracing::debug!("Discovery stderr:\n{}", stderr);
        }

        if !output.status.success() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "Discovery command failed: {}",
                stderr
            )));
        }

        let tests = self.parse_discover_output(&stdout);

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
        let full_command = self.substitute_command(tests, result_path);

        // Run through shell to properly handle quoted arguments, pipes, redirects, etc.
        // This matches the behavior of discover() and avoids issues with split_whitespace()
        // breaking commands like: jest "test with spaces" --reporter="json"
        let mut cmd = Command::new("sh").arg("-c").arg(&full_command);

        if let Some(dir) = &self.config.working_dir {
            cmd = cmd.working_dir(dir.to_string_lossy());
        }

        cmd
    }
}
