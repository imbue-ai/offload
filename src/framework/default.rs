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
//! # Example: Jest
//!
//! ```toml
//! [groups.javascript]
//! type = "default"
//! discover_command = "jest --listTests --json | jq -r '.[]' | xargs -I{} basename {}"
//! run_command = "jest {tests} --ci --reporters=jest-junit"
//! result_file = "junit.xml"
//! ```
//!
//! # Example: Go
//!
//! ```toml
//! [groups.go]
//! type = "default"
//! discover_command = "go test -list '.*' ./... 2>/dev/null | grep -E '^Test'"
//! run_command = "go test -v -run '^({tests})$' ./..."
//! # Note: Go needs go-junit-report for JUnit XML output
//! ```

use std::path::PathBuf;

use async_trait::async_trait;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestOutcome, TestRecord,
    TestResult,
};
use crate::config::DefaultFrameworkConfig;
use crate::provider::{Command, ExecResult};

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

    /// Substitute {tests} placeholder in run command.
    ///
    /// Test IDs are shell-escaped to handle IDs containing spaces or special characters.
    fn substitute_tests(&self, tests: &[TestInstance]) -> String {
        let test_ids: Vec<_> = tests
            .iter()
            .map(|t| shell_words::quote(t.id()).into_owned())
            .collect();
        self.config
            .run_command
            .replace("{tests}", &test_ids.join(" "))
    }
}

#[async_trait]
impl TestFramework for DefaultFramework {
    async fn discover(&self, _paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
        // Run test discovery command through shell to support pipes, globs, etc.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c");
        cmd.arg(&self.config.discover_command);

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

    fn produce_test_execution_command(&self, tests: &[TestInstance]) -> Command {
        let full_command = self.substitute_tests(tests);

        // Run through shell to properly handle quoted arguments, pipes, redirects, etc.
        // This matches the behavior of discover() and avoids issues with split_whitespace()
        // breaking commands like: jest "test with spaces" --reporter="json"
        let mut cmd = Command::new("sh").arg("-c").arg(&full_command);

        if let Some(dir) = &self.config.working_dir {
            cmd = cmd.working_dir(dir.to_string_lossy());
        }

        cmd
    }

    /// Parse test results from execution output.
    ///
    /// # Result Sources (in order of preference)
    ///
    /// 1. **JUnit XML** (`result_file`): Provides per-test results with timing and error details.
    ///    This is the recommended approach for accurate test tracking.
    ///
    /// 2. **Exit code fallback**: If no JUnit XML is available, returns a single aggregate result
    ///    based on the command's exit code. This means:
    ///    - Individual test pass/fail status is lost
    ///    - All tests are reported under a synthetic "all_tests" ID
    ///    - Flaky test detection won't work correctly
    ///
    /// # Recommendation
    ///
    /// Always configure `result_file` pointing to JUnit XML output for accurate results.
    /// Most test frameworks support JUnit XML output:
    /// - Jest: `--reporters=jest-junit`
    /// - Go: `go-junit-report`
    /// - pytest: `--junitxml=results.xml`
    fn parse_results(
        &self,
        output: &ExecResult,
        result_file: Option<&str>,
    ) -> FrameworkResult<Vec<TestResult>> {
        // Try to parse JUnit XML if result file is provided
        if let Some(xml_content) = result_file {
            return parse_junit_xml(xml_content);
        }

        // Fall back to basic success/failure based on exit code.
        // WARNING: This loses per-test granularity. Without JUnit XML, we cannot determine
        // which specific tests passed or failed - only whether the batch as a whole succeeded.
        tracing::warn!(
            "No JUnit XML result file - falling back to exit code. \
             Per-test results will not be tracked. Configure 'result_file' for accurate results."
        );

        if output.success() {
            Ok(vec![TestResult {
                test_id: "all_tests".to_string(),
                outcome: TestOutcome::Passed,
                duration: output.duration,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                error_message: None,
                stack_trace: None,
            }])
        } else {
            Ok(vec![TestResult {
                test_id: "all_tests".to_string(),
                outcome: TestOutcome::Failed,
                duration: output.duration,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                error_message: Some(format!("Exit code: {}", output.exit_code)),
                stack_trace: None,
            }])
        }
    }
}

/// Parse JUnit XML content to extract test results.
fn parse_junit_xml(content: &str) -> FrameworkResult<Vec<TestResult>> {
    use regex::Regex;

    let mut results = Vec::new();

    let testcase_re = Regex::new(
        r#"<testcase[^>]*name="([^"]+)"[^>]*(?:classname="([^"]+)")?[^>]*(?:time="([^"]+)")?[^>]*(?:/>|>([\s\S]*?)</testcase>)"#
    ).map_err(|e| FrameworkError::ParseError(format!("Invalid regex pattern: {}", e)))?;
    let msg_re = Regex::new(r#"message="([^"]*)""#)
        .map_err(|e| FrameworkError::ParseError(format!("Invalid regex pattern: {}", e)))?;

    for cap in testcase_re.captures_iter(content) {
        let name = &cap[1];
        let classname = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let time: f64 = cap
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0.0);
        let inner = cap.get(4).map(|m| m.as_str()).unwrap_or("");

        let test_id = if classname.is_empty() {
            name.to_string()
        } else {
            format!("{}::{}", classname, name)
        };

        let (outcome, error_message) = if inner.contains("<failure") {
            let msg = msg_re.captures(inner).map(|c| c[1].to_string());
            (TestOutcome::Failed, msg)
        } else if inner.contains("<error") {
            let msg = msg_re.captures(inner).map(|c| c[1].to_string());
            (TestOutcome::Error, msg)
        } else if inner.contains("<skipped") {
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
