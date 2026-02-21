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
use tracing::{debug, error, info, warn};

use crate::framework::{TestFramework, TestInstance};
use crate::provider::{OutputLine, Sandbox};
use crate::report::SharedJunitReport;

/// Count testcases in a JUnit XML string.
fn count_testcases_in_xml(xml: &str) -> usize {
    // Count both <testcase .../> (self-closing) and <testcase ...> (with content)
    let self_closing = xml.matches("<testcase ").count();
    self_closing
}

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
///         OutputLine::ExitCode(_) => {}
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
    /// Optional directory to save individual batch JUnit XMLs for debugging.
    parts_dir: Option<std::path::PathBuf>,
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
            parts_dir: None,
        }
    }

    /// Sets the directory for saving JUnit XMLs for debugging.
    ///
    /// When set, each sandbox's junit.xml is saved to `parts_dir/{sandbox_id}.xml`
    /// before being added to the shared report. This allows inspection of
    /// individual batch results.
    pub fn with_parts_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.parts_dir = Some(dir);
        self
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
    ///             OutputLine::ExitCode(_) => {}
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
        let expected_count = tests.len();
        let sandbox_id = self.sandbox.id().to_string();

        info!(
            "[BATCH START] Sandbox {} starting batch with {} tests",
            sandbox_id, expected_count
        );

        // Log all test IDs in this batch
        let test_ids: Vec<_> = tests.iter().map(|t| t.id()).collect();
        debug!(
            "[BATCH TESTS] Sandbox {} test IDs: {:?}",
            sandbox_id, test_ids
        );

        // CHECK FOR DUPLICATES - this would cause pytest to only run the test once!
        let mut seen = std::collections::HashSet::new();
        let mut duplicates = Vec::new();
        for id in &test_ids {
            if !seen.insert(*id) {
                duplicates.push(*id);
            }
        }
        if !duplicates.is_empty() {
            error!(
                "[BATCH DUPLICATES] Sandbox {} has {} DUPLICATE test IDs! Duplicates: {:?}",
                sandbox_id,
                duplicates.len(),
                duplicates
            );
            let unique_count = seen.len();
            warn!(
                "[BATCH DUPLICATES] {} total tests but only {} unique - pytest will only produce {} results!",
                expected_count, unique_count, unique_count
            );
        }

        // Generate the run command for all tests
        let mut cmd = self.framework.produce_test_execution_command(tests);
        cmd = cmd.timeout(self.timeout.as_secs());

        info!(
            "[BATCH EXEC] Sandbox {} executing command for {} tests",
            sandbox_id, expected_count
        );

        // Execute the command with streaming (always use streaming for default provider support)
        let exec_result = match self.exec_with_streaming(&cmd, "batch").await? {
            Some(result) => result,
            None => {
                // Cancelled - return early without recording results
                warn!(
                    "[BATCH CANCELLED] Sandbox {} was cancelled before completion ({} tests lost)",
                    sandbox_id, expected_count
                );
                return Ok(false);
            }
        };

        let duration = start.elapsed();

        info!(
            "[BATCH COMPLETE] Sandbox {} finished execution: exit_code={}, duration={:?}",
            sandbox_id, exec_result.exit_code, duration
        );

        // Calculate unique test count (what pytest will actually produce)
        let unique_test_ids: std::collections::HashSet<_> = test_ids.iter().collect();
        let unique_count = unique_test_ids.len();

        // Download JUnit XML and add to shared report
        match self.try_download_results(unique_count).await {
            Some((xml_content, actual_count)) => {
                info!(
                    "[BATCH RESULTS] Sandbox {} downloaded junit.xml: total={}, unique={}, actual={}, bytes={}",
                    sandbox_id, expected_count, unique_count, actual_count, xml_content.len()
                );

                // CRASH if we got fewer results than unique count (what pytest should produce)
                if actual_count < unique_count {
                    error!(
                        "[BATCH MISMATCH] Sandbox {} has FEWER results than unique tests! unique={}, actual={}",
                        sandbox_id, unique_count, actual_count
                    );
                    error!(
                        "[BATCH MISMATCH] All test IDs ({}): {:?}",
                        test_ids.len(),
                        test_ids
                    );
                    error!(
                        "[BATCH MISMATCH] XML content preview (first 2000 chars):\n{}",
                        &xml_content[..xml_content.len().min(2000)]
                    );
                    panic!(
                        "BATCH RESULT MISMATCH: Sandbox {} expected {} unique tests but got {} in junit.xml",
                        sandbox_id, unique_count, actual_count
                    );
                }

                if let Some(report) = &self.junit_report {
                    match report.lock() {
                        Ok(mut report) => {
                            let before = report.total_count();
                            report.add_junit_xml(&xml_content);
                            let after = report.total_count();
                            info!(
                                "[BATCH ADDED] Sandbox {} added to master report: before={}, after={}, delta={}",
                                sandbox_id, before, after, after - before
                            );
                        }
                        Err(e) => {
                            error!(
                                "[BATCH ERROR] Failed to lock junit report for {}: {}",
                                sandbox_id, e
                            );
                        }
                    }
                } else {
                    warn!("[BATCH WARN] No junit report configured for {}", sandbox_id);
                }
            }
            None => {
                error!(
                    "[BATCH NO RESULTS] Sandbox {} failed to download junit.xml! {} tests lost!",
                    sandbox_id, expected_count
                );
                panic!(
                    "BATCH DOWNLOAD FAILED: Sandbox {} could not download junit.xml for {} tests",
                    sandbox_id, expected_count
                );
            }
        }

        Ok(true)
    }

    /// Try to download JUnit results from the sandbox.
    /// Returns (xml_content, testcase_count) if successful.
    async fn try_download_results(&mut self, expected_count: usize) -> Option<(String, usize)> {
        let sandbox_id = self.sandbox.id().to_string();

        // Debug: List /tmp contents before download
        info!("[DOWNLOAD] Sandbox {} listing /tmp/ contents...", sandbox_id);
        let list_cmd = crate::provider::Command::new("ls").arg("-la").arg("/tmp/");
        if let Ok(mut stream) = self.sandbox.exec_stream(&list_cmd).await {
            use futures::StreamExt;
            let mut tmp_contents = Vec::new();
            while let Some(line) = stream.next().await {
                tmp_contents.push(format!("{:?}", line));
            }
            info!(
                "[DOWNLOAD] Sandbox {} /tmp/ contents: {}",
                sandbox_id,
                tmp_contents.join(" | ")
            );
        }

        // Download from /tmp/junit.xml (standard location)
        let remote_path = std::path::Path::new("/tmp/junit.xml");
        let temp_file = tempfile::NamedTempFile::new().ok()?;

        info!(
            "[DOWNLOAD] Sandbox {} downloading /tmp/junit.xml...",
            sandbox_id
        );
        let path_pairs = [(remote_path, temp_file.path() as &std::path::Path)];
        match self.sandbox.download(&path_pairs).await {
            Ok(_) => info!(
                "[DOWNLOAD] Sandbox {} download succeeded",
                sandbox_id
            ),
            Err(e) => {
                error!(
                    "[DOWNLOAD FAILED] Sandbox {} download failed: {}",
                    sandbox_id, e
                );
                return None;
            }
        }

        let content = match std::fs::read_to_string(temp_file.path()) {
            Ok(c) => c,
            Err(e) => {
                error!(
                    "[DOWNLOAD READ FAILED] Sandbox {} failed to read temp file: {}",
                    sandbox_id, e
                );
                return None;
            }
        };

        if content.is_empty() {
            error!(
                "[DOWNLOAD EMPTY] Sandbox {} downloaded empty junit.xml!",
                sandbox_id
            );
            return None;
        }

        // Count testcases in the XML
        let actual_count = count_testcases_in_xml(&content);
        info!(
            "[DOWNLOAD] Sandbox {} junit.xml: {} bytes, {} testcases (expected {})",
            sandbox_id,
            content.len(),
            actual_count,
            expected_count
        );

        // Save to parts directory for debugging if configured
        if let Some(ref parts_dir) = self.parts_dir {
            if let Err(e) = std::fs::create_dir_all(parts_dir) {
                warn!("Failed to create parts dir {:?}: {}", parts_dir, e);
            } else {
                // Sanitize sandbox ID to be a valid filename
                let safe_id = sandbox_id
                    .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
                let part_file = parts_dir.join(format!("{}.xml", safe_id));
                if let Err(e) = std::fs::write(&part_file, &content) {
                    warn!("Failed to save part file {:?}: {}", part_file, e);
                } else {
                    info!(
                        "[PARTS] Saved {} to {:?} ({} bytes, {} testcases)",
                        sandbox_id,
                        part_file,
                        content.len(),
                        actual_count
                    );
                }
            }

            // Log parts dir stats
            if let Ok(entries) = std::fs::read_dir(parts_dir) {
                let count = entries.filter(|e| e.is_ok()).count();
                info!("[PARTS] Directory now has {} files", count);
            }
        }

        Some((content, actual_count))
    }
}
