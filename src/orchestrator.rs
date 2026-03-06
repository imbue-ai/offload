//! Test orchestration: discovery, scheduling, parallel execution, and result aggregation.
pub mod pool;
pub mod runner;
pub mod scheduler;
pub mod spawn;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::framework::{TestFramework, TestInstance, TestRecord, TestResult};
use crate::provider::Sandbox;
use crate::report::{MasterJunitReport, load_test_durations, print_summary};

pub use pool::SandboxPool;
pub use runner::{BatchOutcome, OutputCallback, TestRunner};
pub use scheduler::Scheduler;

/// Maximum expected duration for a single batch of tests.
///
/// Batches produced by LPT scheduling will not exceed this duration
/// (unless a single test already exceeds it). This keeps batches
/// small enough for fast feedback and efficient retry granularity.
const MAX_BATCH_DURATION: Duration = Duration::from_secs(10);

/// Aggregated results of an entire test run.
///
/// Contains summary statistics and individual test results. This is the
/// return value of [`Orchestrator::run`] and is passed to reporters
/// for final output.
///
/// # Exit Codes
///
/// The [`exit_code`](Self::exit_code) method returns conventional exit codes:
///
/// | Code | Meaning |
/// |------|---------|
/// | 0 | All tests passed |
/// | 1 | Some tests failed or weren't run |
/// | 2 | All tests passed but some were flaky |
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Total number of tests discovered.
    pub total_tests: usize,

    /// Number of tests that passed.
    pub passed: usize,

    /// Number of tests that failed (assertions or errors).
    pub failed: usize,

    /// Number of tests that were skipped.
    pub skipped: usize,

    /// Number of tests that were flaky (passed on retry).
    ///
    /// A flaky test is one that failed initially but passed after retrying.
    pub flaky: usize,

    /// Number of tests that couldn't be run.
    ///
    /// Typically due to sandbox creation failures or infrastructure issues.
    pub not_run: usize,

    /// Wall-clock duration of the entire test run.
    pub duration: Duration,

    /// Individual test results for all executed tests.
    pub results: Vec<TestResult>,
}

impl RunResult {
    /// Returns `true` if the test run was successful.
    ///
    /// A run is successful if no tests failed and all scheduled tests
    /// were executed. Flaky tests are considered successful (they passed
    /// on retry).
    pub fn success(&self) -> bool {
        self.failed == 0 && self.not_run == 0
    }

    /// Returns an appropriate process exit code for this result.
    pub fn exit_code(&self) -> i32 {
        if self.failed > 0 || self.not_run > 0 {
            1
        } else if self.flaky > 0 {
            2 // 2 is the convention that offload has decided to store for flakiness
        } else {
            0
        }
    }
}

/// The main orchestrator that coordinates test execution.
///
/// The orchestrator is the top-level component that ties together:
/// - A pre-populated [`SandboxPool`] of execution environments
/// - A [`TestFramework`] for running tests
///
/// It manages the full lifecycle of a test run: scheduling,
/// parallel execution, retries, and result aggregation.
///
/// # Type Parameters
///
/// - `S`: The sandbox type (implements [`Sandbox`](crate::provider::Sandbox))
/// - `D`: The test framework type
///
pub struct Orchestrator<S, D> {
    config: Config,
    framework: D,
    verbose: bool,
    tracer: crate::trace::Tracer,
    _sandbox: std::marker::PhantomData<S>,
}

impl<S, D> Orchestrator<S, D>
where
    S: Sandbox + 'static,
    D: TestFramework,
{
    /// Creates a new orchestrator with the given components.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration loaded from TOML
    /// * `framework` - Test framework for running tests
    /// * `verbose` - Whether to show verbose output (streaming test output)
    /// * `tracer` - Performance tracer for emitting trace events
    pub fn new(config: Config, framework: D, verbose: bool, tracer: crate::trace::Tracer) -> Self {
        Self {
            config,
            framework,
            verbose,
            tracer,
            _sandbox: std::marker::PhantomData,
        }
    }

    /// Runs the given tests and returns the aggregated results.
    ///
    /// Takes already-discovered tests as input, allowing callers to
    /// inspect or filter tests before execution. Results are recorded
    /// into each `TestRecord` via interior mutability.
    ///
    /// # Arguments
    ///
    /// * `tests` - The tests to run (typically from [`discover`](Self::discover))
    /// * `sandbox_pool` - Pool of sandboxes to use
    ///
    /// # Returns
    ///
    /// [`RunResult`] containing summary statistics and individual results.
    ///
    /// # Errors
    ///
    /// Returns an error if critical infrastructure errors occur.
    pub async fn run_with_tests(
        &self,
        tests: &[TestRecord],
        mut sandbox_pool: SandboxPool<S>,
    ) -> anyhow::Result<RunResult> {
        let start = std::time::Instant::now();

        // Load test durations from previous junit.xml for LPT scheduling
        let _dur_span = self.tracer.span(
            "duration_loading",
            "orchestrator",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        let junit_path = self
            .config
            .report
            .output_dir
            .join(&self.config.report.junit_file);
        let durations = load_test_durations(&junit_path, self.config.framework.test_id_format());
        drop(_dur_span);

        // Ensure output directory exists (don't clear - junit.xml will be overwritten when ready)
        let output_dir = &self.config.report.output_dir;
        std::fs::create_dir_all(output_dir).ok();

        // Clear parts directory from previous run
        let parts_dir = output_dir.join("junit-parts");
        if parts_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&parts_dir) {
                warn!("Failed to clear parts directory: {}", e);
            } else {
                debug!("Cleared parts directory: {:?}", parts_dir);
            }
        }
        std::fs::create_dir_all(&parts_dir).ok();

        if tests.is_empty() {
            warn!("No tests to run");
            return Ok(RunResult {
                total_tests: 0,
                passed: 0,
                failed: 0,
                skipped: 0,
                flaky: 0,
                not_run: 0,
                duration: start.elapsed(),
                results: Vec::new(),
            });
        }

        // Set up progress bar
        let total_instances: usize = tests
            .iter()
            .filter(|t| !t.skipped)
            .map(|t| t.retry_count + 1)
            .sum();
        let progress = indicatif::ProgressBar::new(total_instances as u64);
        if let Ok(style) = indicatif::ProgressStyle::default_bar().template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
        ) {
            progress.set_style(style.progress_chars("#>-"));
        }
        progress.enable_steady_tick(std::time::Duration::from_millis(100));

        // Filter out skipped tests and create Test handles
        // For tests with retry_count > 0, create multiple instances to run in parallel
        let tests_to_run: Vec<TestInstance<'_>> = tests
            .iter()
            .filter(|t| !t.skipped)
            .flat_map(|t| {
                let count = t.retry_count + 1; // 1 original + retry_count retries
                (0..count).map(move |_| t.test())
            })
            .collect();

        let skipped_count = tests.len() - tests.iter().filter(|t| !t.skipped).count();

        // Schedule tests using LPT (Longest Processing Time First) if we have durations,
        // otherwise fall back to round-robin with a warning.
        let _sched_span = self.tracer.span(
            "scheduling",
            "orchestrator",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        let scheduler = Scheduler::new(self.config.offload.max_parallel);
        let batches = if durations.is_empty() {
            warn!(
                "No historical test durations found at {}. Falling back to round-robin scheduling. \
                 Run tests once to generate junit.xml for optimized LPT scheduling.",
                junit_path.display()
            );
            scheduler.schedule(&tests_to_run)
        } else {
            debug!(
                "Using LPT scheduling with {} historical durations from {}",
                durations.len(),
                junit_path.display()
            );
            // Default duration for unknown tests: 1 second (conservative estimate)
            scheduler.schedule_lpt(
                &tests_to_run,
                &durations,
                Duration::from_secs(1),
                Some(MAX_BATCH_DURATION),
            )
        };
        drop(_sched_span);

        // Take sandboxes from pool
        let sandboxes = sandbox_pool.take_all();

        // Log batch distribution
        info!(
            "[ORCHESTRATOR] Scheduled {} tests into {} batches with {} sandboxes",
            tests_to_run.len(),
            batches.len(),
            sandboxes.len()
        );
        for (i, batch) in batches.iter().enumerate() {
            info!("[ORCHESTRATOR] Batch {}: {} tests", i, batch.len());
        }
        let total_in_batches: usize = batches.iter().map(|b| b.len()).sum();
        info!(
            "[ORCHESTRATOR] Total tests across all batches: {} (should equal {})",
            total_in_batches,
            tests_to_run.len()
        );

        // Shared JUnit report for accumulating results and early stopping
        let total_tests_to_run = tests.iter().filter(|t| !t.skipped).count();
        let junit_report = Arc::new(std::sync::Mutex::new(MasterJunitReport::new(
            total_tests_to_run,
        )));
        let all_passed = Arc::new(AtomicBool::new(false));
        let cancellation_token = CancellationToken::new();

        // Collect sandboxes back after use for termination
        let sandboxes_for_cleanup = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Create/truncate logs directory for per-runner output
        let logs_dir = output_dir.join("logs");
        if logs_dir.exists()
            && let Err(e) = std::fs::remove_dir_all(&logs_dir)
        {
            warn!("Failed to clear logs directory: {}", e);
        }
        std::fs::create_dir_all(&logs_dir).ok();

        // Queue-based batching: workers pull from a shared queue
        let queue = Arc::new(std::sync::Mutex::new(VecDeque::from(batches)));
        let batch_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Emit per-sandbox metadata events for trace
        for i in 0..sandboxes.len() {
            let pid = crate::trace::sandbox_pid(i);
            self.tracer.metadata_event(
                "process_name",
                pid,
                crate::trace::TID_API,
                serde_json::json!({"name": format!("Sandbox {}", i)}),
            );
            self.tracer.metadata_event(
                "thread_name",
                pid,
                crate::trace::TID_API,
                serde_json::json!({"name": "API"}),
            );
            self.tracer.metadata_event(
                "thread_name",
                pid,
                crate::trace::TID_EXEC,
                serde_json::json!({"name": "Exec"}),
            );
            self.tracer.metadata_event(
                "thread_name",
                pid,
                crate::trace::TID_IO,
                serde_json::json!({"name": "I/O"}),
            );
        }

        // Run tests in parallel using queue-based workers
        tokio_scoped::scope(|scope| {
            for (sandbox_index, sandbox) in sandboxes.into_iter().enumerate() {
                let cfg = spawn::SpawnConfig {
                    config: &self.config,
                    framework: &self.framework,
                    queue: Arc::clone(&queue),
                    progress: &progress,
                    total_tests_to_run,
                    all_passed: Arc::clone(&all_passed),
                    cancellation_token: cancellation_token.clone(),
                    sandboxes_for_cleanup: Arc::clone(&sandboxes_for_cleanup),
                    junit_report: Arc::clone(&junit_report),
                    logs_dir: logs_dir.clone(),
                    batch_counter: Arc::clone(&batch_counter),
                    verbose: self.verbose,
                    tracer: self.tracer.clone(),
                    sandbox_index,
                };
                scope.spawn(spawn::spawn_task(cfg, sandbox));
            }
        });

        // Aggregate results from TestRecords (handles parallel retries automatically)
        // Get results from the shared JUnit report
        let _agg_span = self.tracer.span(
            "result_aggregation",
            "orchestrator",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        info!("[ORCHESTRATOR] All batches completed, aggregating results...");
        let (passed, failed, flaky_count, total_in_report) = if let Ok(report) = junit_report.lock()
        {
            let summary = report.summary();
            let total = report.total_count();
            info!(
                "[ORCHESTRATOR] Master report: {} total unique tests, {} passed, {} failed, {} flaky",
                total, summary.0, summary.1, summary.2
            );
            (summary.0, summary.1, summary.2, total)
        } else {
            (0, 0, 0, 0)
        };

        // Check for missing tests
        let expected_unique_tests = tests.iter().filter(|t| !t.skipped).count();
        if total_in_report < expected_unique_tests {
            error!(
                "[ORCHESTRATOR MISMATCH] Expected {} unique tests but only {} in report! {} TESTS MISSING!",
                expected_unique_tests,
                total_in_report,
                expected_unique_tests - total_in_report
            );
        } else {
            info!(
                "[ORCHESTRATOR] All {} expected tests accounted for in report",
                expected_unique_tests
            );
        }

        // Write the JUnit report to file
        if self.config.report.junit {
            let output_path = self
                .config
                .report
                .output_dir
                .join(&self.config.report.junit_file);
            if let Ok(report) = junit_report.lock()
                && let Err(e) = report.write_to_file(&output_path)
            {
                warn!("Failed to write JUnit XML: {}", e);
            }
        }

        // Use JUnit report as source of truth for all counts
        let total_discovered = tests.iter().filter(|t| !t.skipped).count();
        let total_in_junit = if let Ok(report) = junit_report.lock() {
            report.total_count()
        } else {
            0
        };
        let not_run = total_discovered.saturating_sub(total_in_junit);

        // Use the JUnit total as the authoritative count (passed + failed + flaky = total)
        // This ensures passed can never exceed total
        let run_result = RunResult {
            total_tests: total_in_junit,
            passed: passed + flaky_count, // Flaky tests count as passed
            failed,
            skipped: skipped_count,
            flaky: flaky_count,
            not_run,
            duration: start.elapsed(),
            results: Vec::new(), // Results are in JUnit XML now
        };
        drop(_agg_span);

        progress.finish_and_clear();
        print_summary(&run_result);

        // Terminate all sandboxes in parallel (fire-and-forget so we don't block the caller)
        let _cleanup_span = self.tracer.span(
            "sandbox_cleanup",
            "orchestrator",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        let sandboxes: Vec<_> = match sandboxes_for_cleanup.lock() {
            Ok(mut guard) => guard.drain(..).collect(),
            Err(e) => {
                error!("sandbox cleanup mutex poisoned: {}", e);
                Vec::new()
            }
        };
        tokio::spawn(async move {
            let terminate_futures = sandboxes.into_iter().map(|sandbox| async move {
                if let Err(e) = sandbox.terminate().await {
                    warn!("Failed to terminate sandbox {}: {}", sandbox.id(), e);
                }
            });
            futures::future::join_all(terminate_futures).await;
            drop(_cleanup_span);
        });

        Ok(run_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CargoFrameworkConfig, FrameworkConfig, GroupConfig, LocalProviderConfig, OffloadConfig,
        ProviderConfig, ReportConfig,
    };
    use crate::framework::TestRecord;
    use crate::provider::{Command, OutputStream, ProviderResult};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Sandbox whose `terminate()` sleeps for a long time, used to verify
    /// that cleanup does not block `run_with_tests`.
    struct SlowTerminateSandbox {
        id: String,
        terminated: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Sandbox for SlowTerminateSandbox {
        fn id(&self) -> &str {
            &self.id
        }

        async fn exec_stream(&self, _cmd: &Command) -> ProviderResult<OutputStream> {
            use crate::provider::OutputLine;
            use futures::stream;
            Ok(Box::pin(stream::iter(vec![OutputLine::ExitCode(0)])))
        }

        async fn upload(&self, _local: &Path, _remote: &Path) -> ProviderResult<()> {
            Ok(())
        }

        async fn download(&self, paths: &[(&Path, &Path)]) -> ProviderResult<()> {
            // Write a minimal JUnit XML with a passing testcase to the local path.
            for &(_remote, local) in paths {
                let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites><testsuite name="suite" tests="1" failures="0">
<testcase classname="offload" name="fake_test" time="0.001"/>
</testsuite></testsuites>"#;
                std::fs::write(local, xml).map_err(crate::provider::ProviderError::Io)?;
            }
            Ok(())
        }

        async fn terminate(&self) -> ProviderResult<()> {
            tokio::time::sleep(Duration::from_secs(3)).await;
            self.terminated.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Minimal framework that generates a no-op command.
    struct FakeFramework;

    #[async_trait]
    impl TestFramework for FakeFramework {
        async fn discover(
            &self,
            _paths: &[PathBuf],
            _filters: &str,
            _group: &str,
        ) -> crate::framework::FrameworkResult<Vec<TestRecord>> {
            Ok(vec![])
        }

        fn produce_test_execution_command(
            &self,
            _tests: &[crate::framework::TestInstance],
            _result_path: &str,
        ) -> Command {
            Command::new("true")
        }
    }

    fn test_config(output_dir: &Path) -> Config {
        Config {
            offload: OffloadConfig {
                max_parallel: 1,
                test_timeout_secs: 30,
                working_dir: None,
                stream_output: false,
                sandbox_project_root: ".".to_string(),
                sandbox_init_cmd: None,
            },
            provider: ProviderConfig::Local(LocalProviderConfig::default()),
            framework: FrameworkConfig::Cargo(CargoFrameworkConfig {
                test_id_format: "{classname} {name}".into(),
                ..Default::default()
            }),
            groups: HashMap::from([(
                "all".to_string(),
                GroupConfig {
                    retry_count: 0,
                    filters: String::new(),
                },
            )]),
            report: ReportConfig {
                output_dir: output_dir.to_path_buf(),
                junit: false,
                junit_file: "junit.xml".into(),
            },
        }
    }

    /// Verify that `run_with_tests` returns promptly without blocking on
    /// sandbox termination. The mock sandbox sleeps 3 seconds in `terminate()`;
    /// the test asserts the method returns in well under that.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_cleanup_does_not_block_return() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let config = test_config(tmp.path());

        let terminated = Arc::new(AtomicBool::new(false));
        let sandbox = SlowTerminateSandbox {
            id: "slow-sandbox-0".to_string(),
            terminated: Arc::clone(&terminated),
        };

        let mut pool = SandboxPool::new();
        pool.add(sandbox);

        let test_record = TestRecord::new("fake_test", "all");
        let tests = vec![test_record];

        let orchestrator: Orchestrator<SlowTerminateSandbox, FakeFramework> =
            Orchestrator::new(config, FakeFramework, false, crate::trace::Tracer::noop());

        let start = std::time::Instant::now();
        let _result = orchestrator.run_with_tests(&tests, pool).await;
        let elapsed = start.elapsed();

        // run_with_tests should return well before the 3-second terminate() sleep.
        assert!(
            elapsed < Duration::from_secs(2),
            "run_with_tests blocked for {:?}; expected < 2s (cleanup should be async)",
            elapsed
        );

        // Termination has not completed yet since it is fire-and-forget.
        assert!(
            !terminated.load(Ordering::SeqCst),
            "terminate() should still be running in the background"
        );

        // Give the background task time to finish so we confirm it does run.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            terminated.load(Ordering::SeqCst),
            "terminate() should have completed in the background"
        );

        Ok(())
    }
}
