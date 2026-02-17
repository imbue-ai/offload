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
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::framework::{TestFramework, TestInstance};
use crate::provider::{OutputLine, Sandbox};
use crate::report::SharedJunitReport;

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
    cancellation_token: Option<CancellationToken>,
    /// Shared JUnit report for accumulating results across batches.
    junit_report: Option<SharedJunitReport>,
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
            cancellation_token: None,
            junit_report: None,
        }
    }

    /// Sets the shared JUnit report for accumulating results.
    ///
    /// # Arguments
    ///
    /// * `report` - Shared report for accumulating JUnit results across batches
    pub fn with_junit_report(mut self, report: SharedJunitReport) -> Self {
        self.junit_report = Some(report);
        self
    }

    /// Sets a cancellation token for early termination.
    ///
    /// When the token is cancelled, the runner will stop waiting for
    /// test output and return early. Used for early stopping when
    /// all tests have passed.
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
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
    /// Returns `Ok(None)` if cancelled before completion.
    async fn exec_with_streaming(
        &mut self,
        cmd: &crate::provider::Command,
        output_id: &str,
    ) -> Result<Option<crate::provider::ExecResult>> {
        let start = std::time::Instant::now();
        let mut stdout = String::new();
        let mut stderr = String::new();

        let mut stream = self.sandbox.exec_stream(cmd).await?;

        // If we have a cancellation token, use select! to race against it
        if let Some(ref token) = self.cancellation_token {
            loop {
                select! {
                    _ = token.cancelled() => {
                        debug!("Test execution cancelled (all tests passed)");
                        return Ok(None);
                    }
                    line = stream.next() => {
                        match line {
                            Some(line) => {
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
                                    OutputLine::ExitCode(_) => {}
                                }
                            }
                            None => break, // Stream ended
                        }
                    }
                }
            }
        } else {
            // No cancellation token, process normally
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
                    OutputLine::ExitCode(_) => {}
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
            return Ok(Some(crate::provider::ExecResult {
                exit_code: parsed.exit_code,
                stdout: parsed.stdout,
                stderr: parsed.stderr,
                duration: start.elapsed(),
            }));
        }

        // Fall back to inferring exit code from output
        let exit_code =
            if stdout.contains("PASSED") && !stdout.contains("FAILED") && !stdout.contains("ERROR")
            {
                0
            } else {
                1
            };

        Ok(Some(crate::provider::ExecResult {
            exit_code,
            stdout,
            stderr,
            duration: start.elapsed(),
        }))
    }

    /// Runs multiple tests in a batch.
    ///
    /// Generates a single command for all tests, executes it, downloads
    /// the JUnit XML results, and adds them to the shared report.
    ///
    /// # Arguments
    ///
    /// * `tests` - The tests to execute as a batch
    ///
    /// # Returns
    ///
    /// `Ok(true)` on successful execution, `Ok(false)` if cancelled early,
    /// or an error if execution failed.
    pub async fn run_tests(&mut self, tests: &[TestInstance<'_>]) -> Result<bool> {
        let start = std::time::Instant::now();

        debug!("Running {} tests", tests.len());

        // Generate the run command for all tests
        let mut cmd = self.framework.produce_test_execution_command(tests);
        cmd = cmd.timeout(self.timeout.as_secs());

        // Execute the command with streaming (always use streaming for default provider support)
        let exec_result = match self.exec_with_streaming(&cmd, "batch").await? {
            Some(result) => result,
            None => {
                // Cancelled - return early without recording results
                debug!("Batch cancelled before completion");
                return Ok(false);
            }
        };

        let duration = start.elapsed();

        debug!(
            "Tests completed with exit code {} in {:?}",
            exec_result.exit_code, duration
        );

        // Download JUnit XML and add to shared report
        if let Some(xml_content) = self.try_download_results().await
            && let Some(report) = &self.junit_report
            && let Ok(mut report) = report.lock()
        {
            report.add_junit_xml(&xml_content);
        }

        Ok(true)
    }

    /// Try to download JUnit results from the sandbox.
    async fn try_download_results(&mut self) -> Option<String> {
        // Debug: List /tmp contents before download
        let list_cmd = crate::provider::Command::new("ls").arg("-la").arg("/tmp/");
        if let Ok(mut stream) = self.sandbox.exec_stream(&list_cmd).await {
            use futures::StreamExt;
            let mut tmp_contents = Vec::new();
            while let Some(line) = stream.next().await {
                tmp_contents.push(format!("{:?}", line));
            }
            tracing::debug!(
                "Sandbox {} /tmp/ contents: {}",
                self.sandbox.id(),
                tmp_contents.join(" | ")
            );
        }

        // Download from /tmp/junit.xml (standard location)
        let remote_path = std::path::Path::new("/tmp/junit.xml");
        let temp_file = tempfile::NamedTempFile::new().ok()?;

        let path_pairs = [(remote_path, temp_file.path() as &std::path::Path)];
        match self.sandbox.download(&path_pairs).await {
            Ok(_) => tracing::debug!(
                "Download of /tmp/junit.xml succeeded for {}",
                self.sandbox.id()
            ),
            Err(e) => tracing::warn!(
                "Download of /tmp/junit.xml failed for {}: {}",
                self.sandbox.id(),
                e
            ),
        }

        let content = std::fs::read_to_string(temp_file.path()).ok()?;
        if content.is_empty() {
            tracing::warn!("Downloaded junit.xml is empty for {}", self.sandbox.id());
            return None;
        }

        tracing::debug!(
            "Downloaded junit.xml has {} bytes for {}",
            content.len(),
            self.sandbox.id()
        );
        Some(content)
    }
}
