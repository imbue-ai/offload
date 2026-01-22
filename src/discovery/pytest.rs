//! pytest test discovery implementation.

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;

use super::{DiscoveryError, DiscoveryResult, TestCase, TestDiscoverer, TestOutcome, TestResult};
use crate::config::PytestDiscoveryConfig;
use crate::provider::{Command, ExecResult};

/// pytest test discoverer.
pub struct PytestDiscoverer {
    config: PytestDiscoveryConfig,
}

impl PytestDiscoverer {
    /// Create a new pytest discoverer with the given configuration.
    pub fn new(config: PytestDiscoveryConfig) -> Self {
        Self { config }
    }

    /// Parse pytest --collect-only output to extract test cases.
    fn parse_collect_output(&self, output: &str) -> Vec<TestCase> {
        let mut tests = Vec::new();

        // Match lines like:
        // <Module tests/test_foo.py>
        //   <Function test_bar>
        //   <Class TestBaz>
        //     <Function test_qux>
        let test_pattern = Regex::new(r"<(?:Function|Method)\s+(\S+)>").unwrap();
        let module_pattern = Regex::new(r"<Module\s+(\S+)>").unwrap();
        let class_pattern = Regex::new(r"<Class\s+(\S+)>").unwrap();

        let mut current_module: Option<String> = None;
        let mut current_class: Option<String> = None;

        for line in output.lines() {
            let trimmed = line.trim();

            if let Some(caps) = module_pattern.captures(trimmed) {
                current_module = Some(caps[1].to_string());
                current_class = None;
            } else if let Some(caps) = class_pattern.captures(trimmed) {
                current_class = Some(caps[1].to_string());
            } else if let Some(caps) = test_pattern.captures(trimmed) {
                let test_name = &caps[1];

                // Build the full test ID
                let test_id = match (&current_module, &current_class) {
                    (Some(module), Some(class)) => {
                        format!("{}::{}::{}", module, class, test_name)
                    }
                    (Some(module), None) => {
                        format!("{}::{}", module, test_name)
                    }
                    _ => test_name.to_string(),
                };

                let mut test = TestCase::new(&test_id);

                // Extract file path from module
                if let Some(module) = &current_module {
                    test = test.with_file(module.clone());
                }

                // Check for skip marker in the line
                if trimmed.contains("skip") {
                    test = test.skipped();
                }

                // Check for flaky marker
                if trimmed.contains("flaky") {
                    test = test.flaky();
                }

                tests.push(test);
            }
        }

        // Also try parsing the simpler format from pytest --collect-only -q
        if tests.is_empty() {
            for line in output.lines() {
                let trimmed = line.trim();
                // Simple format: tests/test_foo.py::test_bar
                if trimmed.contains("::") && !trimmed.starts_with('<') && !trimmed.contains(' ') {
                    tests.push(TestCase::new(trimmed));
                }
            }
        }

        tests
    }
}

#[async_trait]
impl TestDiscoverer for PytestDiscoverer {
    async fn discover(&self, paths: &[PathBuf]) -> DiscoveryResult<Vec<TestCase>> {
        // Build the pytest --collect-only command
        let mut cmd = Command::new(&self.config.python)
            .arg("-m")
            .arg("pytest")
            .arg("--collect-only")
            .arg("-q");

        // Add marker filter if specified
        if let Some(markers) = &self.config.markers {
            cmd = cmd.arg("-m").arg(markers);
        }

        // Add extra args
        for arg in &self.config.extra_args {
            cmd = cmd.arg(arg);
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
            cmd = cmd.arg(path);
        }

        // For local discovery, we run the command directly
        // In the actual execution, the orchestrator will run this in a sandbox
        let output = tokio::process::Command::new(&self.config.python)
            .arg("-m")
            .arg("pytest")
            .arg("--collect-only")
            .arg("-q")
            .args(
                self.config
                    .markers
                    .as_ref()
                    .map(|m| vec!["-m".to_string(), m.clone()])
                    .unwrap_or_default(),
            )
            .args(&self.config.extra_args)
            .args(&search_paths)
            .output()
            .await
            .map_err(|e| DiscoveryError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && !stdout.contains("::") {
            return Err(DiscoveryError::DiscoveryFailed(format!(
                "pytest collection failed: {}",
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

    fn run_command(&self, tests: &[TestCase]) -> Command {
        let mut cmd = Command::new(&self.config.python)
            .arg("-m")
            .arg("pytest")
            .arg("-v")
            .arg("--tb=short")
            .arg("--junitxml=/tmp/junit.xml");

        // Add marker filter if specified
        if let Some(markers) = &self.config.markers {
            cmd = cmd.arg("-m").arg(markers);
        }

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(&test.id);
        }

        cmd
    }

    fn parse_results(
        &self,
        output: &ExecResult,
        result_file: Option<&str>,
    ) -> DiscoveryResult<Vec<TestResult>> {
        let mut results = Vec::new();

        // Try to parse JUnit XML if available
        if let Some(xml_content) = result_file {
            results = parse_junit_xml(xml_content)?;
        }

        // If no JUnit results, parse from stdout
        if results.is_empty() {
            results = parse_pytest_stdout(&output.stdout, &output.stderr)?;
        }

        Ok(results)
    }

    fn name(&self) -> &'static str {
        "pytest"
    }
}

/// Parse JUnit XML content to extract test results.
fn parse_junit_xml(content: &str) -> DiscoveryResult<Vec<TestResult>> {
    // Basic JUnit XML parsing
    // In production, we'd use quick-xml for proper parsing
    let mut results = Vec::new();

    // Simple regex-based parsing for now
    let testcase_re = Regex::new(
        r#"<testcase[^>]*name="([^"]+)"[^>]*classname="([^"]+)"[^>]*time="([^"]+)"[^>]*>"#,
    )
    .unwrap();

    let failure_re = Regex::new(r#"<failure[^>]*message="([^"]*)"[^>]*>"#).unwrap();
    let error_re = Regex::new(r#"<error[^>]*message="([^"]*)"[^>]*>"#).unwrap();
    let skipped_re = Regex::new(r#"<skipped"#).unwrap();

    for cap in testcase_re.captures_iter(content) {
        let name = &cap[1];
        let classname = &cap[2];
        let time: f64 = cap[3].parse().unwrap_or(0.0);

        let test_id = format!("{}::{}", classname.replace('.', "/"), name);
        let test = TestCase::new(&test_id);

        // Find the content between this testcase and the next (or </testcase>)
        let start = cap.get(0).unwrap().end();
        let end = content[start..]
            .find("</testcase>")
            .map(|i| start + i)
            .unwrap_or(content.len());
        let testcase_content = &content[start..end];

        let (outcome, error_message) = if let Some(fail_cap) = failure_re.captures(testcase_content)
        {
            (TestOutcome::Failed, Some(fail_cap[1].to_string()))
        } else if let Some(err_cap) = error_re.captures(testcase_content) {
            (TestOutcome::Error, Some(err_cap[1].to_string()))
        } else if skipped_re.is_match(testcase_content) {
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

/// Parse pytest stdout to extract test results.
fn parse_pytest_stdout(stdout: &str, _stderr: &str) -> DiscoveryResult<Vec<TestResult>> {
    let mut results = Vec::new();

    // Match lines like:
    // tests/test_foo.py::test_bar PASSED
    // tests/test_foo.py::test_baz FAILED
    let result_re = Regex::new(r"(\S+::\S+)\s+(PASSED|FAILED|SKIPPED|ERROR)").unwrap();

    for cap in result_re.captures_iter(stdout) {
        let test_id = &cap[1];
        let status = &cap[2];

        let outcome = match status {
            "PASSED" => TestOutcome::Passed,
            "FAILED" => TestOutcome::Failed,
            "SKIPPED" => TestOutcome::Skipped,
            "ERROR" => TestOutcome::Error,
            _ => continue,
        };

        results.push(TestResult {
            test: TestCase::new(test_id),
            outcome,
            duration: std::time::Duration::ZERO,
            stdout: String::new(),
            stderr: String::new(),
            error_message: None,
            stack_trace: None,
        });
    }

    Ok(results)
}
