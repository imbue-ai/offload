//! Test runner that executes tests in a sandbox.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info};

use crate::discovery::{TestCase, TestDiscoverer, TestOutcome, TestResult};
use crate::provider::DynSandbox;

/// Runs tests in a sandbox.
pub struct TestRunner {
    sandbox: DynSandbox,
    discoverer: Arc<dyn TestDiscoverer>,
    timeout: Duration,
}

impl TestRunner {
    /// Create a new test runner.
    pub fn new(
        sandbox: DynSandbox,
        discoverer: Arc<dyn TestDiscoverer>,
        timeout: Duration,
    ) -> Self {
        Self {
            sandbox,
            discoverer,
            timeout,
        }
    }

    /// Get a reference to the sandbox.
    pub fn sandbox(&self) -> &DynSandbox {
        &self.sandbox
    }

    /// Run a single test and return the result.
    pub async fn run_test(&self, test: &TestCase) -> Result<TestResult> {
        let start = std::time::Instant::now();

        info!("Running test: {}", test.id);

        // Generate the run command
        let mut cmd = self.discoverer.run_command(&[test.clone()]);
        cmd = cmd.timeout(self.timeout.as_secs());

        // Execute the command
        let exec_result = self.sandbox.exec(&cmd).await?;

        let duration = start.elapsed();

        debug!(
            "Test {} completed with exit code {} in {:?}",
            test.id, exec_result.exit_code, duration
        );

        // Try to download and parse JUnit results
        let result_content = self.try_download_results().await;

        // Parse results
        let results = self.discoverer.parse_results(&exec_result, result_content.as_deref())?;

        // Find the result for this specific test
        let test_result = results.into_iter()
            .find(|r| r.test.id == test.id)
            .unwrap_or_else(|| {
                // If we couldn't parse specific results, infer from exit code
                TestResult {
                    test: test.clone(),
                    outcome: if exec_result.success() {
                        TestOutcome::Passed
                    } else {
                        TestOutcome::Failed
                    },
                    duration,
                    stdout: exec_result.stdout.clone(),
                    stderr: exec_result.stderr.clone(),
                    error_message: if !exec_result.success() {
                        Some(format!("Exit code: {}", exec_result.exit_code))
                    } else {
                        None
                    },
                    stack_trace: None,
                }
            });

        Ok(test_result)
    }

    /// Run multiple tests and return all results.
    pub async fn run_tests(&self, tests: &[TestCase]) -> Result<Vec<TestResult>> {
        let start = std::time::Instant::now();

        info!("Running {} tests", tests.len());

        // Generate the run command for all tests
        let mut cmd = self.discoverer.run_command(tests);
        cmd = cmd.timeout(self.timeout.as_secs());

        // Execute the command
        let exec_result = self.sandbox.exec(&cmd).await?;

        let duration = start.elapsed();

        debug!(
            "Tests completed with exit code {} in {:?}",
            exec_result.exit_code, duration
        );

        // Try to download and parse JUnit results
        let result_content = self.try_download_results().await;

        // Parse results
        let mut results = self.discoverer.parse_results(&exec_result, result_content.as_deref())?;

        // If parsing failed, create results based on exit code
        if results.is_empty() {
            let overall_outcome = if exec_result.success() {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            };

            for test in tests {
                results.push(TestResult {
                    test: test.clone(),
                    outcome: overall_outcome,
                    duration: duration / tests.len() as u32,
                    stdout: String::new(),
                    stderr: String::new(),
                    error_message: if !exec_result.success() {
                        Some(format!("Batch failed with exit code: {}", exec_result.exit_code))
                    } else {
                        None
                    },
                    stack_trace: None,
                });
            }
        }

        Ok(results)
    }

    /// Try to download JUnit results from the sandbox.
    async fn try_download_results(&self) -> Option<String> {
        // Common JUnit output locations
        let paths = [
            "/tmp/junit.xml",
            "junit.xml",
            "test-results/junit.xml",
            "target/surefire-reports/TEST-*.xml",
        ];

        for path in &paths {
            let temp_file = tempfile::NamedTempFile::new().ok()?;
            let temp_path = temp_file.path().to_path_buf();

            if self.sandbox.download(std::path::Path::new(path), &temp_path).await.is_ok() {
                if let Ok(content) = std::fs::read_to_string(&temp_path) {
                    if !content.is_empty() {
                        return Some(content);
                    }
                }
            }
        }

        None
    }
}
