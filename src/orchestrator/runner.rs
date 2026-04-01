//! Test runner — executes test batches within a single sandbox.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::framework::{TestFramework, TestInstance};
use crate::provider::retry::with_retry;
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

/// Configuration for downloading artifacts from sandboxes after test execution.
pub struct ArtifactConfig {
    /// Glob patterns for files to download. Empty means no downloads.
    pub globs: Vec<String>,
    /// Base output directory for downloaded artifacts.
    pub output_dir: std::path::PathBuf,
}

/// Configuration shared across all runners in a single Offload run.
pub struct RunnerConfig {
    pub fail_fast: bool,
    pub parts_dir: Option<std::path::PathBuf>,
    pub junit_report: Option<SharedJunitReport>,
    pub cancellation_token: Option<CancellationToken>,
    pub artifacts: ArtifactConfig,
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
    fail_fast: bool,
    /// Index of the current batch (for artifact download directory naming).
    batch_idx: usize,
    /// Configuration for downloading artifacts after batch execution.
    artifact_config: ArtifactConfig,
}

/// Build a `find` command string from glob patterns.
///
/// Converts glob patterns into a `find -path` command. Each pattern is
/// prefixed with `./` if it doesn't already start with `./` or `/`.
fn build_find_command(globs: &[String]) -> String {
    let path_predicates: Vec<String> = globs
        .iter()
        .map(|g| {
            let pattern = if g.starts_with("./") || g.starts_with('/') {
                g.clone()
            } else {
                format!("./{}", g)
            };
            format!("-path '{}'", pattern)
        })
        .collect();

    let find_expr = if path_predicates.len() == 1 {
        path_predicates[0].clone()
    } else {
        format!("\\( {} \\)", path_predicates.join(" -o "))
    };

    format!("find . -type f {}", find_expr)
}

impl<'a, S: Sandbox, D: TestFramework> TestRunner<'a, S, D> {
    /// Creates a new test runner for the given sandbox.
    pub fn new(
        sandbox: S,
        framework: &'a D,
        timeout: Duration,
        tracer: crate::trace::Tracer,
        sandbox_pid: u32,
        batch_idx: usize,
        config: RunnerConfig,
    ) -> Self {
        Self {
            sandbox,
            framework,
            timeout,
            output_callback: None,
            cancellation_token: config.cancellation_token,
            junit_report: config.junit_report,
            parts_dir: config.parts_dir,
            tracer,
            sandbox_pid,
            batch_idx,
            fail_fast: config.fail_fast,
            artifact_config: config.artifacts,
        }
    }

    /// Sets a callback for streaming test output (per-batch, after construction).
    pub fn set_output_callback(&mut self, callback: OutputCallback) {
        self.output_callback = Some(callback);
    }

    /// Consumes the runner and returns the owned sandbox.
    ///
    /// Use this to return the sandbox to a pool for reuse.
    pub fn into_sandbox(self) -> S {
        self.sandbox
    }

    /// Process a single output line from the stream.
    fn process_output_line(
        line: &OutputLine,
        output_id: &str,
        stdout: &mut String,
        stderr: &mut String,
        callback: &Option<OutputCallback>,
    ) {
        match line {
            OutputLine::Stdout(s) => {
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
    }

    /// Execute command with streaming, collecting output.
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
        let mut exit_code: Option<i32> = None;

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
                                if let OutputLine::ExitCode(code) = &line {
                                    exit_code = Some(*code);
                                }
                                Self::process_output_line(
                                    &line,
                                    output_id,
                                    &mut stdout,
                                    &mut stderr,
                                    &self.output_callback,
                                );
                            }
                            None => break, // Stream ended
                        }
                    }
                }
            }
        } else {
            // No cancellation token, process normally
            while let Some(line) = stream.next().await {
                if let OutputLine::ExitCode(code) = &line {
                    exit_code = Some(*code);
                }
                Self::process_output_line(
                    &line,
                    output_id,
                    &mut stdout,
                    &mut stderr,
                    &self.output_callback,
                );
            }
        }

        let exit_code = exit_code.unwrap_or_else(|| {
            warn!("No exit code received from stream, inferring from output");
            if stdout.contains("PASSED") && !stdout.contains("FAILED") && !stdout.contains("ERROR")
            {
                0
            } else {
                1
            }
        });

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
        let result_path = format!("/tmp/{}.{}", sandbox_id, self.framework.report_format());

        // Generate the run command for all tests
        let mut cmd =
            self.framework
                .produce_test_execution_command(tests, &result_path, self.fail_fast);
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
        let batch_had_failures = match self.try_download_results(&result_path, unique_count).await {
            Some((raw_content, _raw_count)) => {
                info!(
                    "[BATCH RESULTS] Sandbox {} downloaded result file: total={}, unique={}, bytes={}",
                    sandbox_id,
                    expected_count,
                    unique_count,
                    raw_content.len()
                );

                // Convert raw output to JUnit XML (vitest produces JSON, others pass through)
                let junit_xml = self
                    .framework
                    .xml_from_report(&raw_content)
                    .map_err(|e| anyhow::anyhow!("Failed to process test results: {}", e))?;

                // Count testcases in the processed JUnit XML
                let processed_count = count_testcases_in_xml(&junit_xml);

                if processed_count < unique_count {
                    // Log warning but don't fail — some frameworks (vitest) filter
                    // non-targeted tests during processing
                    warn!(
                        "[BATCH RESULTS] Sandbox {} expected {} unique tests but got {} after processing",
                        sandbox_id, unique_count, processed_count
                    );
                }

                // Save processed JUnit XML to parts dir (overwrites raw download)
                if let Some(ref parts_dir) = self.parts_dir {
                    let safe_id =
                        sandbox_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
                    let part_file = parts_dir.join(format!("{}.xml", safe_id));
                    if let Err(e) = std::fs::write(&part_file, &junit_xml) {
                        warn!("Failed to save processed part file {:?}: {}", part_file, e);
                    }
                }

                if let Some(report) = &self.junit_report {
                    match report.lock() {
                        Ok(mut report) => {
                            let before = report.total_count();
                            report.add_junit_xml(&junit_xml);
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
                has_failures_in_xml(&junit_xml)
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

        // Download artifacts matching configured glob patterns
        self.try_download_artifacts().await;

        if batch_had_failures {
            Ok(BatchOutcome::Failure)
        } else {
            Ok(BatchOutcome::Success)
        }
    }

    /// Try to download test results from the sandbox.
    /// Returns (content, testcase_count) if successful.
    async fn try_download_results(
        &mut self,
        result_path: &str,
        expected_count: usize,
    ) -> Option<(String, usize)> {
        let sandbox_id = self.sandbox.id().to_string();
        let remote_path = std::path::Path::new(result_path);
        let temp_file = tempfile::NamedTempFile::new().ok()?;

        debug!(
            "[DOWNLOAD] Sandbox {} downloading {}...",
            sandbox_id, result_path
        );
        let path_pairs = [(remote_path, temp_file.path() as &std::path::Path)];
        match with_retry!(self.sandbox.download(&path_pairs)) {
            Ok(_) => debug!("[DOWNLOAD] Sandbox {} download succeeded", sandbox_id),
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
        debug!(
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

    /// Download artifacts matching configured glob patterns from the sandbox.
    ///
    /// Combines `find -print0` and `tar --null -T -` into a single pipeline
    /// exec to avoid ARG_MAX limits and safely handle filenames with newlines
    /// or special characters. Files are stored under
    /// `{output_dir}/{sandbox_id}/{batch_idx}/` preserving relative paths.
    ///
    /// Best-effort: failures are logged as warnings, never fail the batch.
    async fn try_download_artifacts(&mut self) {
        if self.artifact_config.globs.is_empty() {
            return;
        }

        let batch_idx = self.batch_idx;
        let sandbox_id = self.sandbox.id().to_string();
        let safe_id = sandbox_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
        let tar_remote_path = format!("/tmp/offload-artifacts-{}-{}.tar.gz", safe_id, batch_idx);

        // Build a single find+tar pipeline. Uses -print0/--null for safe handling
        // of filenames with newlines or special characters. The pipeline avoids
        // ARG_MAX limits since filenames flow through a pipe, not command args.
        let find_expr = build_find_command(&self.artifact_config.globs);
        let pipeline = format!(
            "{} -print0 | tar czf {} --null -T - 2>/dev/null",
            find_expr, tar_remote_path
        );

        debug!("[ARTIFACTS] Sandbox {} running: {}", sandbox_id, pipeline);

        let tar_cmd = crate::provider::Command::new("sh").arg("-c").arg(&pipeline);

        match self.exec_with_streaming(&tar_cmd, "artifacts").await {
            Ok(Some(_)) => {}
            Ok(None) => {
                debug!("[ARTIFACTS] Sandbox {} cancelled", sandbox_id);
                return;
            }
            Err(e) => {
                warn!("[ARTIFACTS] Sandbox {} find+tar failed: {}", sandbox_id, e);
                return;
            }
        }

        // Download the single tar archive
        let temp_tar = match tempfile::NamedTempFile::new() {
            Ok(f) => f,
            Err(e) => {
                warn!("[ARTIFACTS] Failed to create temp file: {}", e);
                return;
            }
        };

        let tar_remote = std::path::Path::new(&tar_remote_path);
        let download_pairs = [(tar_remote, temp_tar.path() as &std::path::Path)];

        if let Err(e) = self.sandbox.download(&download_pairs).await {
            // Download failure likely means no files matched (tar wasn't created)
            debug!(
                "[ARTIFACTS] Sandbox {} tar download failed (no artifacts?): {}",
                sandbox_id, e
            );
            return;
        }

        // Check if the downloaded file is empty (no matches)
        let tar_size = std::fs::metadata(temp_tar.path())
            .map(|m| m.len())
            .unwrap_or(0);
        if tar_size == 0 {
            debug!(
                "[ARTIFACTS] Sandbox {} no files matched globs {:?}",
                sandbox_id, self.artifact_config.globs
            );
            return;
        }

        let dest_base = self
            .artifact_config
            .output_dir
            .join(&safe_id)
            .join(batch_idx.to_string());

        // Extract tar archive locally
        if let Err(e) = std::fs::create_dir_all(&dest_base) {
            warn!(
                "[ARTIFACTS] Failed to create destination dir {:?}: {}",
                dest_base, e
            );
            return;
        }

        let tar_extract = std::process::Command::new("tar")
            .arg("xzf")
            .arg(temp_tar.path())
            .arg("-C")
            .arg(&dest_base)
            .output();

        match tar_extract {
            Ok(output) if output.status.success() => {
                info!(
                    "[ARTIFACTS] Sandbox {} downloaded artifacts ({} bytes) to {:?}",
                    sandbox_id, tar_size, dest_base
                );
            }
            Ok(output) => {
                warn!(
                    "[ARTIFACTS] Sandbox {} tar extraction failed: {}",
                    sandbox_id,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(e) => {
                warn!(
                    "[ARTIFACTS] Sandbox {} tar extraction error: {}",
                    sandbox_id, e
                );
            }
        }
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

    #[test]
    fn test_build_find_command_single_glob() {
        let cmd = build_find_command(&["*.xml".to_string()]);
        assert_eq!(cmd, "find . -type f -path './*.xml'");
    }

    #[test]
    fn test_build_find_command_multiple_globs() {
        let cmd = build_find_command(&[
            "*.xml".to_string(),
            "*.png".to_string(),
            "coverage/*".to_string(),
        ]);
        assert_eq!(
            cmd,
            "find . -type f \\( -path './*.xml' -o -path './*.png' -o -path './coverage/*' \\)"
        );
    }

    #[test]
    fn test_build_find_command_preserves_leading_dot_slash() {
        let cmd = build_find_command(&["./output/*.log".to_string()]);
        assert_eq!(cmd, "find . -type f -path './output/*.log'");
    }

    #[test]
    fn test_build_find_command_preserves_absolute_path() {
        let cmd = build_find_command(&["/tmp/*.dat".to_string()]);
        assert_eq!(cmd, "find . -type f -path '/tmp/*.dat'");
    }
}
