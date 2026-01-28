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
//!
//! # Run Command Template
//!
//! The `run_command` uses `{tests}` as a placeholder for space-separated
//! test IDs:
//!
//! ```toml
//! run_command = "npm test -- {tests}"
//! # Becomes: npm test -- test_login test_logout
//! ```
//!
//! # Result Parsing
//!
//! For detailed results, configure `result_file` to point to a JUnit XML
//! file produced by the test runner. Without this, results are inferred
//! from exit codes only.
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
    /// use shotgun::framework::default::DefaultFramework;
    /// use shotgun::config::DefaultFrameworkConfig;
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
    fn substitute_tests(&self, tests: &[TestInstance]) -> String {
        let test_ids: Vec<_> = tests.iter().map(|t| t.id()).collect();
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

        // Parse the command into program and args
        let parts: Vec<&str> = full_command.split_whitespace().collect();
        if parts.is_empty() {
            return Command::new("echo").arg("No command specified");
        }

        let mut cmd = Command::new(parts[0]);
        for arg in &parts[1..] {
            cmd = cmd.arg(*arg);
        }

        if let Some(dir) = &self.config.working_dir {
            cmd = cmd.working_dir(dir.to_string_lossy());
        }

        cmd
    }

    fn parse_results(
        &self,
        output: &ExecResult,
        result_file: Option<&str>,
    ) -> FrameworkResult<Vec<TestResult>> {
        // Try to parse JUnit XML if result file is provided
        if let Some(xml_content) = result_file {
            return parse_junit_xml(xml_content);
        }

        // Fall back to basic success/failure based on exit code
        // This is a simple implementation - users should use JUnit XML for detailed results
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
    ).unwrap();
    let msg_re = Regex::new(r#"message="([^"]*)""#).unwrap();

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
