//! Default framework — custom shell commands for test discovery and execution.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord,
    discovery_error_detail,
};
use crate::config::DefaultFrameworkConfig;
use crate::provider::Command;
use crate::report::junit::TestsuiteXml;

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
    fn parse_discover_output(&self, output: &str, group: &str) -> Vec<TestRecord> {
        output
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| TestRecord::new(line, group))
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
        group: &str,
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
            let detail = discovery_error_detail(&stderr, &stdout);
            let cmd_display = format!("sh -c '{}'", discover_command);
            return Err(FrameworkError::DiscoveryFailed(format!(
                "discover_command failed ({}):\n  command: {}\n  {}",
                output.status, cmd_display, detail
            )));
        }

        let tests = self.parse_discover_output(&stdout, group);

        if tests.is_empty() {
            tracing::warn!(
                "No tests discovered. Output: {}",
                discovery_error_detail(&stderr, &stdout)
            );
        }

        Ok(tests)
    }

    fn produce_test_execution_command(
        &self,
        tests: &[TestInstance],
        result_path: &str,
        _fail_fast: bool,
    ) -> Command {
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

    fn resolve_test_ids(
        &self,
        testsuites: &mut [TestsuiteXml],
        batch_test_ids: &[String],
    ) -> FrameworkResult<()> {
        let batch_set: HashSet<&str> = batch_test_ids.iter().map(|s| s.as_str()).collect();
        for testsuite in testsuites.iter_mut() {
            for testcase in &mut testsuite.testcases {
                let canonical = crate::config::format_test_id(
                    &self.config.test_id_format,
                    &testcase.name,
                    testcase.classname.as_deref(),
                );
                if batch_set.contains(canonical.as_str()) {
                    testcase.name = canonical;
                    testcase.classname = None;
                } else {
                    // Fall back to suffix matching when exact format doesn't match
                    match super::resolve_test_id_suffix_matching(
                        &testcase.name,
                        testcase.classname.as_deref(),
                        batch_test_ids,
                    ) {
                        Ok(resolved) => {
                            testcase.name = resolved.to_string();
                            testcase.classname = None;
                        }
                        Err(msg) => {
                            return Err(FrameworkError::Other(anyhow::anyhow!(
                                "JUnit testcase '{}' (format '{}' produced '{}') not resolved: {}",
                                testcase.name,
                                self.config.test_id_format,
                                canonical,
                                msg
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
