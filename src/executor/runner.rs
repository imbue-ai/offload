//! Test runner for executing tests in a sandbox.
//!
//! The [`TestRunner`] is responsible for executing tests within a single
//! sandbox and parsing their results. It handles both single-test and
//! batch execution modes.
//!
//! # Features
//!
//! - **Single test execution**: Run one test at a time with [`run_test`](TestRunner::run_test)
//! - **Batch execution**: Run multiple tests with [`run_tests`](TestRunner::run_tests)
//! - **Streaming output**: Real-time output via callback with [`with_streaming`](TestRunner::with_streaming)
//! - **Result parsing**: Automatic parsing via the discoverer
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use shotgun::executor::TestRunner;
//! use shotgun::provider::process::ProcessSandbox;
//! use shotgun::discovery::pytest::PytestDiscoverer;
//! use shotgun::discovery::TestCase;
//!
//! async fn run_test_example(
//!     sandbox: ProcessSandbox,
//!     discoverer: &PytestDiscoverer,
//! ) -> anyhow::Result<()> {
//!     let mut runner = TestRunner::new(sandbox, discoverer, Duration::from_secs(300));
//!
//!     let test = TestCase::new("tests/test_math.py::test_add");
//!     let result = runner.run_test(&test).await?;
//!
//!     if result.outcome.is_success() {
//!         println!("Test passed!");
//!     }
//!     Ok(())
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use tracing::{debug, info};

use crate::discovery::{TestCase, TestDiscoverer, TestOutcome, TestResult};
use crate::provider::{OutputLine, Sandbox};

/// Callback function for streaming test output.
///
/// Called for each line of output during streaming execution. The callback
/// receives the test ID and the output line.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use shotgun::executor::OutputCallback;
/// use shotgun::provider::OutputLine;
///
/// let callback: OutputCallback = Arc::new(|test_id, line| {
///     match line {
///         OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
///         OutputLine::Stderr(s) => eprintln!("[{}] {}", test_id, s),
///     }
/// });
/// ```
pub type OutputCallback = Arc<dyn Fn(&str, &OutputLine) + Send + Sync>;

/// Executes tests within a single sandbox.
///
/// The runner handles command generation, execution, output capture,
/// and result parsing. It uses the configured discoverer to generate
/// appropriate commands and parse results.
///
/// # Type Parameters
///
/// - `S`: The sandbox type (implements [`Sandbox`])
/// - `D`: The discoverer type (implements [`TestDiscoverer`])
pub struct TestRunner<'a, S, D> {
    sandbox: S,
    discoverer: &'a D,
    timeout: Duration,
    stream_output: bool,
    output_callback: Option<OutputCallback>,
}

impl<'a, S: Sandbox, D: TestDiscoverer> TestRunner<'a, S, D> {
    /// Creates a new test runner for the given sandbox.
    ///
    /// # Arguments
    ///
    /// * `sandbox` - The sandbox to execute tests in
    /// * `discoverer` - The discoverer for command generation and result parsing
    /// * `timeout` - Maximum time for test execution
    pub fn new(sandbox: S, discoverer: &'a D, timeout: Duration) -> Self {
        Self {
            sandbox,
            discoverer,
            timeout,
            stream_output: false,
            output_callback: None,
        }
    }

    /// Enables streaming output with a callback.
    ///
    /// When enabled, test output is sent to the callback as it occurs
    /// rather than being buffered.
    ///
    /// # Arguments
    ///
    /// * `callback` - Function called for each line of output
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use shotgun::executor::TestRunner;
    /// use shotgun::provider::OutputLine;
    /// # use shotgun::provider::process::ProcessSandbox;
    /// # use shotgun::discovery::pytest::PytestDiscoverer;
    /// # use std::time::Duration;
    ///
    /// # fn example(sandbox: ProcessSandbox, discoverer: &PytestDiscoverer) {
    /// let runner = TestRunner::new(sandbox, discoverer, Duration::from_secs(300))
    ///     .with_streaming(Arc::new(|test_id, line| {
    ///         match line {
    ///             OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
    ///             OutputLine::Stderr(s) => eprintln!("[{}] ERR: {}", test_id, s),
    ///         }
    ///     }));
    /// # }
    /// ```
    pub fn with_streaming(mut self, callback: OutputCallback) -> Self {
        self.stream_output = true;
        self.output_callback = Some(callback);
        self
    }

    /// Returns a reference to the underlying sandbox.
    ///
    /// Useful for terminating the sandbox after tests complete.
    pub fn sandbox(&self) -> &S {
        &self.sandbox
    }

    /// Runs a single test and returns its result.
    ///
    /// Generates a command for the test using the discoverer, executes it
    /// in the sandbox, and parses the results.
    ///
    /// # Arguments
    ///
    /// * `test` - The test case to execute
    ///
    /// # Returns
    ///
    /// The test result including outcome, duration, and captured output.
    pub async fn run_test(&mut self, test: &TestCase) -> Result<TestResult> {
        let start = std::time::Instant::now();

        info!("Running test: {}", test.id);

        // Generate the run command
        let mut cmd = self.discoverer.run_command(std::slice::from_ref(test));
        cmd = cmd.timeout(self.timeout.as_secs());

        // Execute the command (streaming or buffered)
        let exec_result = if self.stream_output {
            self.exec_streaming(&cmd, &test.id).await?
        } else {
            self.sandbox.exec(&cmd).await?
        };

        let duration = start.elapsed();

        debug!(
            "Test {} completed with exit code {} in {:?}",
            test.id, exec_result.exit_code, duration
        );

        // Try to download and parse JUnit results
        let result_content = self.try_download_results().await;

        // Parse results
        let results = self
            .discoverer
            .parse_results(&exec_result, result_content.as_deref())?;

        // Find the result for this specific test
        let test_result = results
            .into_iter()
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

    /// Execute command with streaming output.
    async fn exec_streaming(
        &mut self,
        cmd: &crate::provider::Command,
        test_id: &str,
    ) -> Result<crate::provider::ExecResult> {
        let start = std::time::Instant::now();
        let mut stdout = String::new();
        let mut stderr = String::new();

        let mut stream = self.sandbox.exec_stream(cmd).await?;

        while let Some(line) = stream.next().await {
            // Call the output callback if set
            if let Some(ref callback) = self.output_callback {
                callback(test_id, &line);
            }

            // Collect output
            match &line {
                OutputLine::Stdout(s) => {
                    stdout.push_str(s);
                    stdout.push('\n');
                }
                OutputLine::Stderr(s) => {
                    stderr.push_str(s);
                    stderr.push('\n');
                }
            }
        }

        // Infer exit code from output (streaming doesn't give us exit code directly)
        // Look for pytest/test framework exit patterns
        let exit_code =
            if stdout.contains("PASSED") && !stdout.contains("FAILED") && !stdout.contains("ERROR")
            {
                0
            } else if stdout.contains("FAILED")
                || stdout.contains("ERROR")
                || stdout.contains("error")
                || stderr.contains("error")
            {
                1
            } else {
                1 // Assume failure if no clear success indicators (safer default)
            };

        Ok(crate::provider::ExecResult {
            exit_code,
            stdout,
            stderr,
            duration: start.elapsed(),
        })
    }

    /// Runs multiple tests in a batch and returns all results.
    ///
    /// Generates a single command for all tests, executes it, and parses
    /// the combined results. More efficient than running tests individually
    /// but provides less isolation.
    ///
    /// # Arguments
    ///
    /// * `tests` - The test cases to execute as a batch
    ///
    /// # Returns
    ///
    /// Results for all tests. If parsing fails, infers results from the
    /// overall exit code.
    pub async fn run_tests(&mut self, tests: &[TestCase]) -> Result<Vec<TestResult>> {
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
        let mut results = self
            .discoverer
            .parse_results(&exec_result, result_content.as_deref())?;

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
                        Some(format!(
                            "Batch failed with exit code: {}",
                            exec_result.exit_code
                        ))
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
    async fn try_download_results(&mut self) -> Option<String> {
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

            if self
                .sandbox
                .download(std::path::Path::new(path), &temp_path)
                .await
                .is_ok()
            {
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
