//! Test runner for executing tests in a sandbox.
//!
//! The [`TestRunner`] is responsible for executing tests within a single
//! sandbox and parsing their results.
//!
//! # Features
//!
//! - **Test execution**: Run tests with [`run_tests`](TestRunner::run_tests)
//! - **Output callback**: Real-time output via callback with [`with_output_callback`](TestRunner::with_output_callback)
//! - **Result parsing**: Automatic parsing via the framework
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use offload::orchestrator::TestRunner;
//! use offload::provider::local::LocalSandbox;
//! use offload::framework::pytest::PytestFramework;
//! use offload::framework::TestRecord;
//!
//! async fn run_test_example(
//!     sandbox: LocalSandbox,
//!     framework: &PytestFramework,
//! ) -> anyhow::Result<()> {
//!     let mut runner = TestRunner::new(sandbox, framework, Duration::from_secs(300));
//!
//!     let record = TestRecord::new("tests/test_math.py::test_add");
//!     let test = record.test();
//!     runner.run_tests(&[test]).await?;
//!
//!     // Results are stored in the TestRecord
//!     if let Some(result) = record.final_result() {
//!         if result.outcome.is_success() {
//!             println!("Test passed!");
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use tracing::{debug, info};

use crate::framework::{TestFramework, TestInstance, TestOutcome, TestResult};
use crate::provider::{OutputLine, Sandbox};

/// JSON result format from default provider sandboxes.
#[derive(serde::Deserialize)]
struct JsonExecResult {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Callback function for streaming test output.
///
/// Called for each line of output during streaming execution. The callback
/// receives the test ID and the output line.
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use offload::orchestrator::OutputCallback;
/// use offload::provider::OutputLine;
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
/// and result parsing. It uses the configured framework to generate
/// appropriate commands and parse results.
///
/// # Type Parameters
///
/// - `S`: The sandbox type (implements [`Sandbox`])
/// - `D`: The framework type (implements [`TestFramework`])
pub struct TestRunner<'a, S, D> {
    sandbox: S,
    framework: &'a D,
    timeout: Duration,
    output_callback: Option<OutputCallback>,
}

impl<'a, S: Sandbox, D: TestFramework> TestRunner<'a, S, D> {
    /// Creates a new test runner for the given sandbox.
    ///
    /// # Arguments
    ///
    /// * `sandbox` - The sandbox to execute tests in
    /// * `framework` - The framework for command generation and result parsing
    /// * `timeout` - Maximum time for test execution
    pub fn new(sandbox: S, framework: &'a D, timeout: Duration) -> Self {
        Self {
            sandbox,
            framework,
            timeout,
            output_callback: None,
        }
    }

    /// Sets a callback for test output.
    ///
    /// When set, test output is sent to the callback as it occurs.
    /// This is useful for real-time output display.
    ///
    /// # Arguments
    ///
    /// * `callback` - Function called for each line of output
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use offload::orchestrator::TestRunner;
    /// use offload::provider::OutputLine;
    /// # use offload::provider::local::LocalSandbox;
    /// # use offload::framework::pytest::PytestFramework;
    /// # use std::time::Duration;
    ///
    /// # fn example(sandbox: LocalSandbox, framework: &PytestFramework) {
    /// let runner = TestRunner::new(sandbox, framework, Duration::from_secs(300))
    ///     .with_output_callback(Arc::new(|test_id, line| {
    ///         match line {
    ///             OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
    ///             OutputLine::Stderr(s) => eprintln!("[{}] ERR: {}", test_id, s),
    ///         }
    ///     }));
    /// # }
    /// ```
    pub fn with_output_callback(mut self, callback: OutputCallback) -> Self {
        self.output_callback = Some(callback);
        self
    }

    /// Returns a reference to the underlying sandbox.
    ///
    /// Useful for terminating the sandbox after tests complete.
    pub fn sandbox(&self) -> &S {
        &self.sandbox
    }

    /// Consumes the runner and returns the owned sandbox.
    ///
    /// Use this to return the sandbox to a pool for reuse.
    pub fn into_sandbox(self) -> S {
        self.sandbox
    }

    /// Execute command with streaming, collecting output and parsing JSON result.
    ///
    /// The `output_id` is passed to the output callback to identify the source.
    async fn exec_with_streaming(
        &mut self,
        cmd: &crate::provider::Command,
        output_id: &str,
    ) -> Result<crate::provider::ExecResult> {
        let start = std::time::Instant::now();
        let mut stdout = String::new();
        let mut stderr = String::new();

        let mut stream = self.sandbox.exec_stream(cmd).await?;

        while let Some(line) = stream.next().await {
            // Call the output callback if set
            if let Some(ref callback) = self.output_callback {
                callback(output_id, &line);
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

        // Try to parse JSON result from stdout (default provider protocol)
        // This handles default sandboxes that return JSON with exit_code, stdout, stderr
        if let Some(json_line) = stdout
            .lines()
            .rev()
            .find(|line| line.trim().starts_with('{'))
            && let Ok(parsed) = serde_json::from_str::<JsonExecResult>(json_line)
        {
            return Ok(crate::provider::ExecResult {
                exit_code: parsed.exit_code,
                stdout: parsed.stdout,
                stderr: parsed.stderr,
                duration: start.elapsed(),
            });
        }

        // Fall back to inferring exit code from output
        let exit_code =
            if stdout.contains("PASSED") && !stdout.contains("FAILED") && !stdout.contains("ERROR")
            {
                0
            } else {
                1
            };

        Ok(crate::provider::ExecResult {
            exit_code,
            stdout,
            stderr,
            duration: start.elapsed(),
        })
    }

    /// Runs multiple tests in a batch and records results into TestRecords.
    ///
    /// Generates a single command for all tests, executes it, and parses
    /// the combined results. More efficient than running tests individually
    /// but provides less isolation. Results are automatically recorded into
    /// each test's associated [`TestRecord`].
    ///
    /// # Arguments
    ///
    /// * `tests` - The tests to execute as a batch
    ///
    /// # Returns
    ///
    /// `Ok(())` on successful execution, or an error if execution failed.
    /// Use [`TestRecord::final_result`] to retrieve individual test results.
    pub async fn run_tests(&mut self, tests: &[TestInstance<'_>]) -> Result<()> {
        let start = std::time::Instant::now();

        info!("Running {} tests", tests.len());

        // Generate the run command for all tests
        let mut cmd = self.framework.produce_test_execution_command(tests);
        cmd = cmd.timeout(self.timeout.as_secs());

        // Execute the command with streaming (always use streaming for default provider support)
        let exec_result = self.exec_with_streaming(&cmd, "batch").await?;

        let duration = start.elapsed();

        debug!(
            "Tests completed with exit code {} in {:?}",
            exec_result.exit_code, duration
        );

        // Try to download and parse JUnit results
        let result_content = self.try_download_results().await;

        // Parse results
        let parsed_results = self
            .framework
            .parse_results(&exec_result, result_content.as_deref())?;

        // Estimate per-test duration from wall-clock time
        let estimated_duration = duration / tests.len() as u32;

        // Record results into each TestRecord
        for test in tests {
            let mut result = parsed_results
                .iter()
                .find(|r| r.test_id == test.id())
                .cloned()
                .unwrap_or_else(|| {
                    // If no parsed result for this test, infer from exit code
                    let overall_outcome = if exec_result.success() {
                        TestOutcome::Passed
                    } else {
                        TestOutcome::Failed
                    };
                    TestResult {
                        test_id: test.id_owned(),
                        outcome: overall_outcome,
                        duration: estimated_duration,
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
                    }
                });

            // If parsed result has zero duration, use estimated wall-clock duration
            if result.duration.is_zero() {
                result.duration = estimated_duration;
            }

            test.record_result(result);
        }

        Ok(())
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
                && let Ok(content) = std::fs::read_to_string(&temp_path)
                && !content.is_empty()
            {
                return Some(content);
            }
        }

        None
    }
}
