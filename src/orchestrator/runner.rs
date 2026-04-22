//! Test runner — executes test batches within a single sandbox.

use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::framework::{TestFramework, TestInstance};
use crate::orchestrator::completion::SharedCompletionTracker;
use crate::provider::retry::with_retry;
use crate::provider::{OutputLine, Sandbox};
use crate::report::{SharedJunitReport, parse_all_testsuites_xml};

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
pub type OutputCallback = Box<dyn FnMut(&str, &OutputLine) + Send>;

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

/// Outcome of a streaming command execution.
enum ExecStreamOutcome {
    /// Command completed (possibly with non-zero exit code).
    Completed(crate::provider::ExecResult),
    /// Execution was cancelled via the cancellation token.
    Cancelled,
    /// Command exceeded its timeout and was killed.
    TimedOut,
}

/// Configuration for downloading artifacts from sandboxes after test execution.
pub struct ArtifactConfig {
    /// Glob patterns for files to download. Empty means no downloads.
    pub globs: Vec<String>,
    /// Base output directory for downloaded artifacts.
    pub output_dir: std::path::PathBuf,
    /// When true, only download artifacts when the batch had failures.
    pub on_failure_only: bool,
}

/// Configuration shared across all runners in a single Offload run.
pub struct RunnerConfig {
    pub fail_fast: bool,
    pub parts_dir: std::path::PathBuf,
    pub junit_report: SharedJunitReport,
    pub tracker: SharedCompletionTracker,
    pub cancellation_token: CancellationToken,
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
    cancellation_token: CancellationToken,
    /// Shared JUnit report for accumulating results across batches.
    junit_report: SharedJunitReport,
    /// Directory to save individual batch JUnit XMLs for debugging.
    parts_dir: std::path::PathBuf,
    tracer: crate::trace::Tracer,
    sandbox_pid: u32,
    fail_fast: bool,
    /// Index of the current batch (for artifact download directory naming).
    batch_idx: usize,
    /// Configuration for downloading artifacts after batch execution.
    artifact_config: ArtifactConfig,
    /// Shared completion tracker for decided-outcome counting.
    tracker: SharedCompletionTracker,
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
            tracker: config.tracker,
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
        callback: &mut Option<OutputCallback>,
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
    async fn exec_with_streaming(
        &mut self,
        cmd: &crate::provider::Command,
        output_id: &str,
    ) -> Result<ExecStreamOutcome> {
        let start = std::time::Instant::now();
        let mut stdout = String::new();
        let mut stderr = String::new();

        let timeout_duration = cmd
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::MAX);
        let timeout_sleep = tokio::time::sleep(timeout_duration);
        tokio::pin!(timeout_sleep);

        let (mut stream, mut child) = self.sandbox.exec_stream(cmd).await?;

        loop {
            select! {
                _ = self.cancellation_token.cancelled() => {
                    debug!("Test execution cancelled (all tests passed)");
                    return Ok(ExecStreamOutcome::Cancelled);
                    // child is dropped here, killing the process (kill_on_drop)
                }
                _ = &mut timeout_sleep => {
                    let secs = timeout_duration.as_secs();
                    warn!("Test execution timed out after {}s", secs);
                    // child is dropped here, killing the process (kill_on_drop)
                    return Ok(ExecStreamOutcome::TimedOut);
                }
                line = stream.next() => {
                    match line {
                        Some(line) => {
                            Self::process_output_line(
                                &line,
                                output_id,
                                &mut stdout,
                                &mut stderr,
                                &mut self.output_callback,
                            );
                        }
                        None => break, // Stream ended
                    }
                }
            }
        }

        let exit_code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };

        Ok(ExecStreamOutcome::Completed(crate::provider::ExecResult {
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
    pub async fn run_tests(&mut self, tests: &[TestInstance]) -> Result<BatchOutcome> {
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
            ExecStreamOutcome::Completed(result) => result,
            ExecStreamOutcome::Cancelled => {
                // Cancelled - return early without recording results
                info!(
                    "[BATCH CANCELLED] Sandbox {} was cancelled before completion ({} tests lost)",
                    sandbox_id, expected_count
                );
                return Ok(BatchOutcome::Cancelled);
            }
            ExecStreamOutcome::TimedOut if tests.len() == 1 => {
                // Singleton batch timeout: record the test as a failure in the
                // completion tracker so it counts as decided.
                let test_id = tests[0].id();
                warn!(
                    "[BATCH TIMEOUT] Sandbox {} singleton test '{}' timed out after {}s, recording as failure",
                    sandbox_id,
                    test_id,
                    self.timeout.as_secs()
                );
                if let Ok(mut t) = self.tracker.lock() {
                    t.record_batch(&[test_id], |_| false);
                }
                return Ok(BatchOutcome::Failure);
            }
            ExecStreamOutcome::TimedOut => {
                return Err(anyhow::anyhow!(
                    "Test execution timed out after {}s",
                    self.timeout.as_secs()
                ));
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
        let (junit_xml, batch_had_failures) = match self
            .try_download_results(&result_path, unique_count)
            .await
        {
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
                {
                    let safe_id =
                        sandbox_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
                    let part_file = self.parts_dir.join(format!("{}.xml", safe_id));
                    if let Err(e) = std::fs::write(&part_file, &junit_xml) {
                        warn!("Failed to save processed part file {:?}: {}", part_file, e);
                    }
                }

                let batch_had_failures = has_failures_in_xml(&junit_xml);
                (junit_xml, batch_had_failures)
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
        if !self.artifact_config.on_failure_only || batch_had_failures {
            self.try_download_artifacts().await;
        }

        // Parse JUnit XML into testsuites and resolve test IDs using the framework
        let batch_ids: Vec<String> = tests.iter().map(|t| t.id().to_string()).collect();
        let mut testsuites = parse_all_testsuites_xml(&junit_xml);

        if testsuites.is_empty() {
            warn!(
                "[BATCH WARN] Sandbox {} parsed 0 testsuites from JUnit XML ({} bytes)",
                sandbox_id,
                junit_xml.len()
            );
        } else if let Err(e) = self.framework.resolve_test_ids(&mut testsuites, &batch_ids) {
            error!(
                "[BATCH ERROR] Sandbox {} failed to resolve test IDs: {}",
                sandbox_id, e
            );
            return Ok(BatchOutcome::Failure);
        }

        // Bookkeeping: update the master JUnit report and completion tracker
        // after artifacts have been downloaded.
        match self.junit_report.lock() {
            Ok(mut report) => {
                let before = report.total_count();
                if let Err(e) = report.add_junit_xml(testsuites) {
                    error!(
                        "[BATCH ERROR] Sandbox {} failed to add testsuites: {}",
                        sandbox_id, e
                    );
                    return Ok(BatchOutcome::Failure);
                }
                let after = report.total_count();
                info!(
                    "[BATCH ADDED] Sandbox {} added to master report: before={}, after={}, delta={}",
                    sandbox_id,
                    before,
                    after,
                    after - before
                );

                // Update completion tracker immediately so progress
                // stays in sync with the report.
                if let Ok(mut t) = self.tracker.lock() {
                    t.record_batch(
                        &batch_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        |id| report.has_test_passed(id),
                    );
                }
            }
            Err(e) => {
                error!(
                    "[BATCH ERROR] Failed to lock junit report for {}: {}",
                    sandbox_id, e
                );
            }
        }

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

        // Download directly into parts_dir.
        if let Err(e) = std::fs::create_dir_all(&self.parts_dir) {
            warn!("Failed to create parts dir {:?}: {}", self.parts_dir, e);
            return None;
        }
        let safe_id = sandbox_id.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
        let download_path = self.parts_dir.join(format!("{}.xml", safe_id));

        debug!(
            "[DOWNLOAD] Sandbox {} downloading {}...",
            sandbox_id, result_path
        );
        let path_pairs = [(remote_path, download_path.as_path())];
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

        let content = match std::fs::read_to_string(&download_path) {
            Ok(c) => c,
            Err(e) => {
                error!(
                    "[DOWNLOAD READ FAILED] Sandbox {} failed to read downloaded file: {}",
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

        info!(
            "[PARTS] Saved {} to {:?} ({} bytes, {} testcases)",
            sandbox_id,
            download_path,
            content.len(),
            actual_count
        );

        // Log parts dir stats
        if let Ok(entries) = std::fs::read_dir(download_path.parent().unwrap_or(&download_path)) {
            let count = entries.filter(|e| e.is_ok()).count();
            info!("[PARTS] Directory now has {} files", count);
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
            Ok(ExecStreamOutcome::Completed(_)) => {}
            Ok(ExecStreamOutcome::Cancelled) => {
                debug!("[ARTIFACTS] Sandbox {} cancelled", sandbox_id);
                return;
            }
            Ok(ExecStreamOutcome::TimedOut) => {
                warn!("[ARTIFACTS] Sandbox {} find+tar timed out", sandbox_id);
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
    use std::sync::{Arc, Mutex};

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

    /// Minimal framework stub for testing the runner without a real framework.
    struct StubFramework;

    #[async_trait::async_trait]
    impl crate::framework::TestFramework for StubFramework {
        async fn discover(
            &self,
            _paths: &[std::path::PathBuf],
            _filters: &str,
            _group: &str,
        ) -> crate::framework::FrameworkResult<Vec<crate::framework::TestRecord>> {
            Ok(vec![])
        }

        fn produce_test_execution_command(
            &self,
            _tests: &[crate::framework::TestInstance],
            _result_path: &str,
            _fail_fast: bool,
        ) -> crate::provider::Command {
            crate::provider::Command::new("true")
        }

        fn resolve_test_ids(
            &self,
            _testsuites: &mut [crate::report::junit::TestsuiteXml],
            _batch_test_ids: &[String],
        ) -> crate::framework::FrameworkResult<()> {
            Ok(())
        }
    }

    /// Helper to create a LocalSandbox via the provider.
    async fn create_test_sandbox(id: &str) -> Result<crate::provider::local::LocalSandbox> {
        use crate::config::LocalProviderConfig;
        use crate::provider::SandboxProvider;

        let provider_config = LocalProviderConfig {
            working_dir: None,
            env: std::collections::HashMap::new(),
            shell: "/bin/sh".to_string(),
        };
        let provider = crate::provider::local::LocalProvider::new(provider_config);
        let config = crate::config::SandboxConfig {
            id: id.to_string(),
            working_dir: Some(".".to_string()),
            env: vec![],
            copy_dirs: vec![],
        };
        Ok(provider.create_sandbox(&config).await?)
    }

    /// Helper to build a RunnerConfig for tests.
    fn test_runner_config(parts_dir: &std::path::Path) -> RunnerConfig {
        let tracker = Arc::new(Mutex::new(
            crate::orchestrator::completion::CompletionTracker::new(0),
        ));
        let junit_report = Arc::new(Mutex::new(crate::report::junit::MasterJunitReport::new(0)));
        RunnerConfig {
            fail_fast: false,
            parts_dir: parts_dir.to_path_buf(),
            junit_report,
            tracker,
            cancellation_token: CancellationToken::new(),
            artifacts: ArtifactConfig {
                globs: vec![],
                output_dir: parts_dir.to_path_buf(),
                on_failure_only: false,
            },
        }
    }

    #[tokio::test]
    async fn test_exec_with_streaming_times_out() -> Result<()> {
        let sandbox = create_test_sandbox("timeout-test").await?;
        let framework = StubFramework;
        let parts_dir = std::env::temp_dir().join("offload-test-timeout-parts");
        let _ = std::fs::create_dir_all(&parts_dir);

        let mut runner = TestRunner::new(
            sandbox,
            &framework,
            Duration::from_secs(2),
            crate::trace::Tracer::noop(),
            0,
            0,
            test_runner_config(&parts_dir),
        );

        // Command with a 2-second timeout running sleep 60
        let cmd = crate::provider::Command::new("sleep").arg("60").timeout(2);

        let result = runner.exec_with_streaming(&cmd, "timeout-test").await?;

        assert!(
            matches!(result, ExecStreamOutcome::TimedOut),
            "Expected ExecStreamOutcome::TimedOut"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_exec_with_streaming_no_timeout_when_none() -> Result<()> {
        let sandbox = create_test_sandbox("no-timeout-test").await?;
        let framework = StubFramework;
        let parts_dir = std::env::temp_dir().join("offload-test-no-timeout-parts");
        let _ = std::fs::create_dir_all(&parts_dir);

        let mut runner = TestRunner::new(
            sandbox,
            &framework,
            Duration::from_secs(900),
            crate::trace::Tracer::noop(),
            0,
            0,
            test_runner_config(&parts_dir),
        );

        // Command with no timeout that completes quickly
        let cmd = crate::provider::Command::new("echo").arg("hello");

        let result = runner.exec_with_streaming(&cmd, "no-timeout-test").await?;

        match result {
            ExecStreamOutcome::Completed(exec_result) => {
                assert_eq!(exec_result.exit_code, 0);
                assert!(exec_result.stdout.contains("hello"));
            }
            ExecStreamOutcome::Cancelled => {
                return Err(anyhow::anyhow!("Expected Completed, got Cancelled"));
            }
            ExecStreamOutcome::TimedOut => {
                return Err(anyhow::anyhow!("Expected Completed, got TimedOut"));
            }
        }
        Ok(())
    }

    /// Framework stub that produces a long-running command (for timeout tests).
    struct TimeoutStubFramework;

    #[async_trait::async_trait]
    impl crate::framework::TestFramework for TimeoutStubFramework {
        async fn discover(
            &self,
            _paths: &[std::path::PathBuf],
            _filters: &str,
            _group: &str,
        ) -> crate::framework::FrameworkResult<Vec<crate::framework::TestRecord>> {
            Ok(vec![])
        }

        fn produce_test_execution_command(
            &self,
            _tests: &[crate::framework::TestInstance],
            _result_path: &str,
            _fail_fast: bool,
        ) -> crate::provider::Command {
            crate::provider::Command::new("sleep").arg("60")
        }

        fn resolve_test_ids(
            &self,
            _testsuites: &mut [crate::report::junit::TestsuiteXml],
            _batch_test_ids: &[String],
        ) -> crate::framework::FrameworkResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_singleton_batch_timeout_records_failure() -> Result<()> {
        let sandbox = create_test_sandbox("singleton-timeout").await?;
        let framework = TimeoutStubFramework;
        let parts_dir = std::env::temp_dir().join("offload-test-singleton-timeout-parts");
        let _ = std::fs::create_dir_all(&parts_dir);

        let tracker = Arc::new(Mutex::new(
            crate::orchestrator::completion::CompletionTracker::new(1),
        ));
        let config = RunnerConfig {
            fail_fast: false,
            parts_dir: parts_dir.to_path_buf(),
            junit_report: Arc::new(Mutex::new(crate::report::junit::MasterJunitReport::new(0))),
            tracker: Arc::clone(&tracker),
            cancellation_token: CancellationToken::new(),
            artifacts: ArtifactConfig {
                globs: vec![],
                output_dir: parts_dir.to_path_buf(),
                on_failure_only: false,
            },
        };

        let mut runner = TestRunner::new(
            sandbox,
            &framework,
            Duration::from_secs(2),
            crate::trace::Tracer::noop(),
            0,
            0,
            config,
        );

        let test_record = crate::framework::TestRecord {
            id: "my_test::timeout_case".to_string(),
            name: "timeout_case".to_string(),
            file: None,
            retry_count: 0,
            group: "default".to_string(),
            schedule_individual: false,
        };
        let test_instance = crate::framework::TestInstance::new(&test_record);

        let outcome = runner.run_tests(&[test_instance]).await?;

        assert_eq!(
            outcome,
            BatchOutcome::Failure,
            "Expected BatchOutcome::Failure for singleton timeout"
        );

        // Verify the completion tracker recorded the test as decided.
        let t = tracker.lock().map_err(|e| anyhow::anyhow!("{}", e))?;
        assert!(
            t.is_decided("my_test::timeout_case"),
            "Expected test to be decided in completion tracker"
        );

        Ok(())
    }
}
