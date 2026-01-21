//! Generic test discovery implementation.
//!
//! This discoverer allows users to define custom discovery and run commands
//! for any test framework.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{DiscoveryError, DiscoveryResult, TestCase, TestDiscoverer, TestOutcome, TestResult};
use crate::config::GenericDiscoveryConfig;
use crate::provider::{Command, ExecResult};

/// Generic test discoverer that uses user-provided commands.
pub struct GenericDiscoverer {
    config: GenericDiscoveryConfig,
}

impl GenericDiscoverer {
    /// Create a new generic discoverer with the given configuration.
    pub fn new(config: GenericDiscoveryConfig) -> Self {
        Self { config }
    }

    /// Parse discovery command output to extract test cases.
    ///
    /// Expects one test ID per line.
    fn parse_discover_output(&self, output: &str) -> Vec<TestCase> {
        output
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| TestCase::new(line))
            .collect()
    }

    /// Substitute {tests} placeholder in run command.
    fn substitute_tests(&self, tests: &[TestCase]) -> String {
        let test_ids: Vec<_> = tests.iter().map(|t| t.id.as_str()).collect();
        self.config.run_command.replace("{tests}", &test_ids.join(" "))
    }
}

#[async_trait]
impl TestDiscoverer for GenericDiscoverer {
    async fn discover(&self, _paths: &[PathBuf]) -> DiscoveryResult<Vec<TestCase>> {
        // Run discovery command through shell to support pipes, globs, etc.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c");
        cmd.arg(&self.config.discover_command);

        if let Some(dir) = &self.config.working_dir {
            cmd.current_dir(dir);
        }

        let output = cmd.output().await
            .map_err(|e| DiscoveryError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(DiscoveryError::DiscoveryFailed(format!(
                "Discovery command failed: {}",
                stderr
            )));
        }

        let tests = self.parse_discover_output(&stdout);

        if tests.is_empty() {
            tracing::warn!("No tests discovered. stdout: {}, stderr: {}", stdout, stderr);
        }

        Ok(tests)
    }

    fn run_command(&self, tests: &[TestCase]) -> Command {
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

    fn parse_results(&self, output: &ExecResult, result_file: Option<&str>) -> DiscoveryResult<Vec<TestResult>> {
        // Try to parse JUnit XML if result file is provided
        if let Some(xml_content) = result_file {
            return parse_junit_xml(xml_content);
        }

        // Fall back to basic success/failure based on exit code
        // This is a simple implementation - users should use JUnit XML for detailed results
        if output.success() {
            Ok(vec![TestResult {
                test: TestCase::new("all_tests"),
                outcome: TestOutcome::Passed,
                duration: output.duration,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                error_message: None,
                stack_trace: None,
            }])
        } else {
            Ok(vec![TestResult {
                test: TestCase::new("all_tests"),
                outcome: TestOutcome::Failed,
                duration: output.duration,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                error_message: Some(format!("Exit code: {}", output.exit_code)),
                stack_trace: None,
            }])
        }
    }

    fn name(&self) -> &'static str {
        "generic"
    }
}

/// Parse JUnit XML content to extract test results.
fn parse_junit_xml(content: &str) -> DiscoveryResult<Vec<TestResult>> {
    use regex::Regex;

    let mut results = Vec::new();

    let testcase_re = Regex::new(
        r#"<testcase[^>]*name="([^"]+)"[^>]*(?:classname="([^"]+)")?[^>]*(?:time="([^"]+)")?[^>]*(?:/>|>([\s\S]*?)</testcase>)"#
    ).unwrap();

    for cap in testcase_re.captures_iter(content) {
        let name = &cap[1];
        let classname = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let time: f64 = cap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0.0);
        let inner = cap.get(4).map(|m| m.as_str()).unwrap_or("");

        let test_id = if classname.is_empty() {
            name.to_string()
        } else {
            format!("{}::{}", classname, name)
        };

        let test = TestCase::new(&test_id);

        let (outcome, error_message) = if inner.contains("<failure") {
            let msg_re = Regex::new(r#"message="([^"]*)""#).unwrap();
            let msg = msg_re.captures(inner).map(|c| c[1].to_string());
            (TestOutcome::Failed, msg)
        } else if inner.contains("<error") {
            let msg_re = Regex::new(r#"message="([^"]*)""#).unwrap();
            let msg = msg_re.captures(inner).map(|c| c[1].to_string());
            (TestOutcome::Error, msg)
        } else if inner.contains("<skipped") {
            (TestOutcome::Skipped, None)
        } else {
            (TestOutcome::Passed, None)
        };

        results.push(TestResult {
            test,
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
