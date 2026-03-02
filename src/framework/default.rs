//! Default test framework implementation.
//!
//! This framework enables integration with any test framework by using
//! custom shell commands for test discovery and execution.
//!
//! # When to Use
//!
//! Use the default framework when:
//! - Your framework isn't directly supported (Jest, Mocha, Go, etc.)
//! - You have custom test organization that standard frameworks don't handle
//! - You need specialized test discovery logic
//!
//! # Platform Requirements
//!
//! **POSIX shell required**: All commands are executed via `sh -c "command"`.
//! This works on Linux, macOS, and Windows with WSL or Git Bash.
//! Native Windows `cmd.exe` is not supported.
//!
//! # Discovery Protocol
//!
//! The `discover_command` should output test IDs to stdout, one per line:
//! ```text
//! test_login
//! test_logout
//! test_user_profile
//! ```
//!
//! Lines starting with `#` are treated as comments and ignored.
//! Empty lines and leading/trailing whitespace are also ignored.
//!
//! # Run Command Template
//!
//! The `run_command` uses `{tests}` as a placeholder for test IDs.
//! Test IDs are shell-escaped and space-separated:
//!
//! ```toml
//! run_command = "npm test -- {tests}"
//! # Becomes: npm test -- test_login test_logout
//! # With special chars: npm test -- 'test with spaces' 'test$special'
//! ```
//!
//! The entire command runs through `sh -c`, so pipes, redirects, and
//! shell features work correctly.
//!
//! # Result Parsing
//!
//! ## Recommended: JUnit XML
//!
//! Configure `result_file` to point to a JUnit XML file produced by the
//! test runner. This provides:
//! - Per-test pass/fail status
//! - Timing information
//! - Error messages and stack traces
//! - Proper flaky test detection
//!
//! ## Fallback: Exit Code Only
//!
//! Without `result_file`, results are inferred from exit codes:
//! - Exit 0 → all tests passed
//! - Exit non-zero → all tests failed
//!
//! **Limitations of exit code fallback:**
//! - Cannot identify which specific tests failed
//! - All tests reported under synthetic "all_tests" ID
//! - Flaky test detection will not work
//! - Retry behavior may be incorrect
//!
//! # JUnit XML Parsing
//!
//! The built-in JUnit XML parser handles common formats but has limitations:
//! - Uses regex-based parsing (not a full XML parser)
//! - May fail on malformed XML or unusual attribute ordering
//! - CDATA sections are not specially handled
//! - Nested testsuites may not be fully supported
//!
//! For complex JUnit output, consider preprocessing or using a dedicated parser.
//!
//! # Group-Level Filters
//!
//! The `discover_command` must contain a `{filters}` placeholder. Each group's
//! `filters` string is substituted into this placeholder during discovery,
//! allowing different groups to discover different subsets of tests:
//!
//! ```toml
//! [framework]
//! type = "default"
//! discover_command = "pytest --collect-only -q {filters} 2>/dev/null | grep '::'"
//! run_command = "pytest -v --junitxml={result_file} {tests}"
//! test_id_format = "{name}"
//!
//! [groups.unit]
//! retry_count = 0
//! filters = "-m 'not slow'"
//!
//! [groups.slow]
//! retry_count = 2
//! filters = "-m 'slow'"
//! ```
//!
//! # Example: Jest
//!
//! ```toml
//! [framework]
//! type = "default"
//! discover_command = "jest --listTests --json {filters} | jq -r '.[]'"
//! run_command = "jest {tests} --ci --reporters=jest-junit"
//! result_file = "junit.xml"
//! test_id_format = "{name}"
//! ```
//!
//! # Example: Go
//!
//! ```toml
//! [framework]
//! type = "default"
//! discover_command = "go test -list '.*' {filters} ./... 2>/dev/null | grep -E '^Test'"
//! run_command = "go test -v -run '^({tests})$' ./..."
//! test_id_format = "{classname}/{name}"
//! ```

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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::framework::default::DefaultFramework;
    /// use offload::config::DefaultFrameworkConfig;
    ///
    /// let framework = DefaultFramework::new(DefaultFrameworkConfig {
    ///     discover_command: "find tests -name '*.test.js' -exec basename {} \\;".into(),
    ///     run_command: "jest {tests}".into(),
    ///     result_file: Some("junit.xml".into()),
    ///     working_dir: None,
    ///     test_id_format: "{name}".into(),
    /// });
    /// ```
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
