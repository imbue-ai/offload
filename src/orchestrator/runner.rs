//! Test runner — executes test batches within a single sandbox.

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
    xml.matches("<testcase ").count()
}

/// Check if a JUnit XML string contains any test failures or errors.
fn has_failures_in_xml(xml: &str) -> bool {
    xml.contains("<failure") || xml.contains("<error")
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
pub type OutputCallback = Arc<dyn Fn(&str, &OutputLine) + Send + Sync>;

/// Outcome of executing a single batch of tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Execution completed; all tests in the batch passed.
    Success,
    /// Execution completed; one or more tests in the batch failed.
    Failure,
    /// Batch was cancelled before completion (e.g., early stopping).
    Cancelled,
}

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
    tracer: crate::trace::Tracer,
    sandbox_pid: u32,
}

impl<'a, S: Sandbox, D: TestFramework> TestRunner<'a, S, D> {
    /// Creates a new test runner for the given sandbox.
    ///
    /// # Arguments
    ///
    /// * `sandbox` - The sandbox to execute tests in
    /// * `framework` - The framework for command generation and result parsing
    /// * `timeout` - Maximum time for test execution
    pub fn new(
        sandbox: S,
        framework: &'a D,
        timeout: Duration,
        tracer: crate::trace::Tracer,
        sandbox_pid: u32,
    ) -> Self {
        Self {
            sandbox,
            framework,
            timeout,
            output_callback: None,
            cancellation_token: None,
            junit_report: None,
            parts_dir: None,
            tracer,
            sandbox_pid,
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
    /// Process a single output line from the stream.
    ///
    /// If the line is a JSON-encoded `JsonExecResult` (the default provider
    /// protocol), decode it and emit the contained stdout/stderr lines to the
    /// callback individually. Otherwise pass the line through as-is.
    ///
    /// Returns `Some(JsonExecResult)` when a JSON result was decoded, so the
    /// caller can use the parsed exit code and skip heuristic inference.
    fn process_output_line(
        line: &OutputLine,
        output_id: &str,
        stdout: &mut String,
        stderr: &mut String,
        callback: &Option<OutputCallback>,
    ) -> Option<JsonExecResult> {
        match line {
            OutputLine::Stdout(s) => {
                // Try to decode the default provider JSON protocol
                if s.trim().starts_with('{')
                    && let Ok(parsed) = serde_json::from_str::<JsonExecResult>(s)
                {
                    // Emit decoded stdout lines to callback
                    if let Some(cb) = callback {
                        for decoded_line in parsed.stdout.lines() {
                            cb(output_id, &OutputLine::Stdout(decoded_line.to_string()));
                        }
                        for decoded_line in parsed.stderr.lines() {
                            cb(output_id, &OutputLine::Stderr(decoded_line.to_string()));
                        }
                    }
                    stdout.push_str(&parsed.stdout);
                    stderr.push_str(&parsed.stderr);
                    return Some(parsed);
                }
                // Not JSON — pass through as-is
                if let Some(cb) = callback {
                    cb(output_id, line);
                }
                stdout.push_str(s);
                stdout.push('\n');
            }
            OutputLine::Stderr(s) => {
                if let Some(cb) = callback {
                    cb(output_id, line);
                }
                stderr.push_str(s);
                stderr.push('\n');
            }
            OutputLine::ExitCode(_) => {}
        }
        None
    }

    async fn exec_with_streaming(
        &mut self,
        cmd: &crate::provider::Command,
        output_id: &str,
    ) -> Result<Option<crate::provider::ExecResult>> {
        let start = std::time::Instant::now();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut json_result: Option<JsonExecResult> = None;

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
                                if let Some(parsed) = Self::process_output_line(
                                    &line,
                                    output_id,
                                    &mut stdout,
                                    &mut stderr,
                                    &self.output_callback,
                                ) {
                                    json_result = Some(parsed);
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
                if let Some(parsed) = Self::process_output_line(
                    &line,
                    output_id,
                    &mut stdout,
                    &mut stderr,
                    &self.output_callback,
                ) {
                    json_result = Some(parsed);
                }
            }
        }

        // Use parsed JSON result if available (default provider protocol)
        if let Some(parsed) = json_result {
            return Ok(Some(crate::provider::ExecResult {
                exit_code: parsed.exit_code,
                stdout,
                stderr,
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
    /// - `Ok(BatchOutcome::Success)` if execution completed and all tests passed
    /// - `Ok(BatchOutcome::Failure)` if execution completed but one or more tests failed
    /// - `Ok(BatchOutcome::Cancelled)` if the batch was cancelled before completion
    /// - `Err(...)` if execution failed due to an infrastructure error
    pub async fn run_tests(&mut self, tests: &[TestInstance<'_>]) -> Result<BatchOutcome> {
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

        // Generate a unique result path per sandbox to avoid collisions
        let result_path = format!("/tmp/{}.xml", sandbox_id);

        // Generate the run command for all tests
        let mut cmd = self
            .framework
            .produce_test_execution_command(tests, &result_path);
        cmd = cmd.timeout(self.timeout.as_secs());

        info!(
            "[BATCH EXEC] Sandbox {} executing command for {} tests: {}",
            sandbox_id,
            expected_count,
            cmd.to_shell_string()
        );

        // Execute the command with streaming (always use streaming for default provider support)
        let _exec_span = self.tracer.span(
            "exec_batch",
            "exec",
            self.sandbox_pid,
            crate::trace::TID_EXEC,
        );
        let exec_result = match self.exec_with_streaming(&cmd, "batch").await? {
            Some(result) => result,
            None => {
                // Cancelled - return early without recording results
                warn!(
                    "[BATCH CANCELLED] Sandbox {} was cancelled before completion ({} tests lost)",
                    sandbox_id, expected_count
                );
                return Ok(BatchOutcome::Cancelled);
            }
        };
        drop(_exec_span);

        let duration = start.elapsed();

        info!(
            "[BATCH COMPLETE] Sandbox {} finished execution: exit_code={}, duration={:?}",
            sandbox_id, exec_result.exit_code, duration
        );

        // Calculate unique test count (what pytest will actually produce)
        let unique_test_ids: std::collections::HashSet<_> = test_ids.iter().collect();
        let unique_count = unique_test_ids.len();

        // Download JUnit XML and add to shared report
        let _io_span = self.tracer.span(
            "download_results",
            "io",
            self.sandbox_pid,
            crate::trace::TID_IO,
        );
        let batch_had_failures = match self.try_download_results(unique_count).await {
            Some((xml_content, actual_count)) => {
                info!(
                    "[BATCH RESULTS] Sandbox {} downloaded junit.xml: total={}, unique={}, actual={}, bytes={}",
                    sandbox_id,
                    expected_count,
                    unique_count,
                    actual_count,
                    xml_content.len()
                );

                // Fail if we got fewer results than unique count (what pytest should produce)
                if actual_count < unique_count {
                    return Err(anyhow::anyhow!(
                        "Sandbox {} expected {} unique tests but got {} in junit.xml",
                        sandbox_id,
                        unique_count,
                        actual_count
                    ));
                }

                if let Some(report) = &self.junit_report {
                    match report.lock() {
                        Ok(mut report) => {
                            let before = report.total_count();
                            report.add_junit_xml(&xml_content);
                            let after = report.total_count();
                            info!(
                                "[BATCH ADDED] Sandbox {} added to master report: before={}, after={}, delta={}",
                                sandbox_id,
                                before,
                                after,
                                after - before
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
                has_failures_in_xml(&xml_content)
            }
            None => {
                return Err(anyhow::anyhow!(
                    "Sandbox {} failed to download junit.xml for {} tests",
                    sandbox_id,
                    expected_count
                ));
            }
        };
        drop(_io_span);

        if batch_had_failures {
            Ok(BatchOutcome::Failure)
        } else {
            Ok(BatchOutcome::Success)
        }
    }

    /// Try to download JUnit results from the sandbox.
    /// Returns (xml_content, testcase_count) if successful.
    async fn try_download_results(&mut self, expected_count: usize) -> Option<(String, usize)> {
        let sandbox_id = self.sandbox.id().to_string();

        // Debug: List /tmp contents before download
        info!(
            "[DOWNLOAD] Sandbox {} listing /tmp/ contents...",
            sandbox_id
        );
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

        // Download from /tmp/{sandbox_id}.xml (unique per sandbox to avoid collisions)
        let remote_path_str = format!("/tmp/{}.xml", sandbox_id);
        let remote_path = std::path::Path::new(&remote_path_str);
        let temp_file = tempfile::NamedTempFile::new().ok()?;

        info!(
            "[DOWNLOAD] Sandbox {} downloading {}...",
            sandbox_id, remote_path_str
        );
        let path_pairs = [(remote_path, temp_file.path() as &std::path::Path)];
        match self.sandbox.download(&path_pairs).await {
            Ok(_) => info!("[DOWNLOAD] Sandbox {} download succeeded", sandbox_id),
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
                let safe_id =
                    sandbox_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_failures_in_xml_no_failures() {
        let xml = r#"<testsuite><testcase name="t1" /></testsuite>"#;
        assert!(!has_failures_in_xml(xml));
    }

    #[test]
    fn test_has_failures_in_xml_with_failure() {
        let xml = r#"<testsuite><testcase name="t1"><failure message="oops">trace</failure></testcase></testsuite>"#;
        assert!(has_failures_in_xml(xml));
    }

    #[test]
    fn test_has_failures_in_xml_with_error() {
        let xml = r#"<testsuite><testcase name="t1"><error message="boom">trace</error></testcase></testsuite>"#;
        assert!(has_failures_in_xml(xml));
    }
}
