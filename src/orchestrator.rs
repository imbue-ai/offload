//! Test orchestration: discovery, scheduling, parallel execution, and result aggregation.
pub mod completion;
pub mod pool;
pub mod runner;
pub mod scheduler;
pub mod spawn;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::{Config, RecordHistory, format_test_id};
use crate::framework::{TestFramework, TestInstance, TestRecord};
use crate::history::{TestAttemptResult, TestHistoryStore, store::JsonlHistoryStore};
use crate::provider::{CostEstimate, Sandbox};
use crate::report::{MasterJunitReport, SharedJunitReport, load_test_durations, print_summary};

pub use pool::SandboxPool;
pub use runner::{BatchOutcome, OutputCallback, TestRunner};
pub use scheduler::Scheduler;

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

    /// Estimated cost of the test run (aggregated from all sandboxes).
    pub estimated_cost: CostEstimate,
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
    config_filename: String,
    run_id: String,
    framework: D,
    verbose: bool,
    tracer: crate::trace::Tracer,
    show_cost: bool,
    fail_fast: bool,
    ci: bool,
    record_history_flag: bool,
    _sandbox: std::marker::PhantomData<S>,
}

impl<S, D> Orchestrator<S, D>
where
    S: Sandbox,
    D: TestFramework,
{
    /// Creates a new orchestrator with the given components.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration loaded from TOML
    /// * `config_filename` - Name of the config file (for history recording)
    /// * `run_id` - Unique identifier for this run (for history recording)
    /// * `framework` - Test framework for running tests
    /// * `verbose` - Whether to show verbose output (streaming test output)
    /// * `tracer` - Performance tracer for emitting trace events
    /// * `show_cost` - Whether to display cost estimate in summary
    /// * `fail_fast` - Whether to stop on first test failure
    /// * `ci` - Whether to use CI mode (plain-text log lines instead of progress bars)
    /// * `record_history_flag` - Whether `--record-history` was passed on the CLI
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        config_filename: String,
        run_id: String,
        framework: D,
        verbose: bool,
        tracer: crate::trace::Tracer,
        show_cost: bool,
        fail_fast: bool,
        ci: bool,
        record_history_flag: bool,
    ) -> Self {
        Self {
            config,
            config_filename,
            run_id,
            framework,
            verbose,
            tracer,
            show_cost,
            fail_fast,
            ci,
            record_history_flag,
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

        // Load test durations for LPT scheduling
        // When history is enabled and the file exists, use history-based durations.
        // Otherwise fall back to junit.xml.
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
        let durations = if let Some(ref history_cfg) = self.config.history {
            let history = JsonlHistoryStore::load(
                &history_cfg.path,
                history_cfg.reservoir_size,
                history_cfg.default_duration_secs,
            )
            .ok();

            if let Some(store) = history {
                let history_durations = store.get_scheduling_durations(&self.config_filename);
                if !history_durations.is_empty() {
                    debug!(
                        "Using history-based scheduling with {} durations from {}",
                        history_durations.len(),
                        history_cfg.path.display()
                    );
                    history_durations
                } else {
                    load_test_durations(&junit_path)
                }
            } else {
                load_test_durations(&junit_path)
            }
        } else {
            load_test_durations(&junit_path)
        };
        drop(_dur_span);

        // Ensure output directory exists
        let output_dir = &self.config.report.output_dir;
        std::fs::create_dir_all(output_dir).ok();

        // Clean output directory from previous run, preserving only the junit XML
        // (which was already read above for LPT scheduling)
        let junit_filename = &self.config.report.junit_file;
        if let Ok(entries) = std::fs::read_dir(output_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && name == junit_filename
                {
                    continue;
                }
                let path = entry.path();
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(()) => debug!("Removed previous output: {}", path.display()),
                    Err(e) => debug!("Failed to remove {}: {}", path.display(), e),
                }
            }
        }

        // Create subdirectories needed for this run
        let parts_dir = output_dir.join("junit-parts");
        std::fs::create_dir_all(&parts_dir).ok();
        let logs_dir = output_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).ok();

        if tests.is_empty() {
            warn!("No tests to run");
            return Ok(RunResult {
                total_tests: 0,
                passed: 0,
                failed: 0,
                flaky: 0,
                not_run: 0,
                duration: start.elapsed(),
                estimated_cost: CostEstimate::default(),
            });
        }

        // Set up progress bar (tracks unique test results, not retry instances)
        let progress = if self.ci {
            indicatif::ProgressBar::hidden()
        } else {
            let pb = indicatif::ProgressBar::new(tests.len() as u64);
            if let Ok(style) = indicatif::ProgressStyle::default_bar().template(
                "{msg}\n{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}%",
            ) {
                pb.set_style(style.progress_chars("#>-"));
            }
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb.set_message(format!(
                "{}\n{}\n{}\n{}",
                console::style("passed: 0").green(),
                console::style("failed: 0").red(),
                console::style("flaky: 0").yellow(),
                console::style(format!("awaiting: {}", tests.len())).dim(),
            ));
            pb
        };

        if self.ci {
            eprintln!(
                "[ci] 0% | passed: 0, failed: 0, flaky: 0, awaiting: {}",
                tests.len()
            );
        }

        // Create test instances
        // For tests with retry_count > 0, create multiple instances to run in parallel
        let (individual_tests, normal_tests): (Vec<_>, Vec<_>) =
            tests.iter().partition(|t| t.schedule_individual);

        // Normal tests: flat_map as before
        let normal_instances: Vec<TestInstance> = normal_tests
            .iter()
            .flat_map(|t| {
                let count = t.retry_count + 1;
                (0..count).map(move |_| t.test())
            })
            .collect();

        // Individually-scheduled tests: round-robin interleave instances across unique tests
        // Produces [A, B, C, A, B, C, A] instead of [A, A, A, B, B, C]
        // so the scheduler sees them already interleaved and preserves order.
        let individual_instances: Vec<TestInstance> = {
            let max_count = individual_tests
                .iter()
                .map(|t| t.retry_count + 1)
                .max()
                .unwrap_or(0);
            let mut instances = Vec::new();
            for round in 0..max_count {
                for test in &individual_tests {
                    if round < test.retry_count + 1 {
                        instances.push(test.test());
                    }
                }
            }
            instances
        };

        // Individually-scheduled instances come first so the scheduler sees them first
        let mut tests_to_run = individual_instances;
        tests_to_run.extend(normal_instances);

        // Schedule tests using LPT (Longest Processing Time First) if we have durations,
        // otherwise fall back to round-robin with a warning.
        let _sched_span = self.tracer.span(
            "scheduling",
            "orchestrator",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        if durations.is_empty() {
            if self.config.history.is_some() {
                info!(
                    "No historical test durations found. Using default durations for scheduling.",
                );
            } else {
                info!(
                    "No historical test durations found at {}. Using default durations for scheduling.",
                    junit_path.display()
                );
            }
        } else {
            debug!(
                "Using LPT scheduling with {} historical durations from {}",
                durations.len(),
                junit_path.display()
            );
        }
        // Compute per-group average durations for tests without historical data
        let group_to_default_duration = {
            let mut group_totals: HashMap<String, (Duration, usize)> = HashMap::new();
            for test in &tests_to_run {
                if let Some(&d) = durations.get(test.id()) {
                    let entry = group_totals
                        .entry(test.group().to_string())
                        .or_insert((Duration::ZERO, 0));
                    entry.0 += d;
                    entry.1 += 1;
                }
            }
            group_totals
                .into_iter()
                .map(|(group, (total, count))| (group, total / count as u32))
                .collect::<HashMap<String, Duration>>()
        };
        let scheduler = Scheduler::new(
            self.config.offload.max_parallel,
            &tests_to_run,
            &durations,
            &group_to_default_duration,
        );
        drop(_sched_span);

        // Take sandboxes from pool
        let sandboxes = sandbox_pool.take_all();

        // Log batch distribution
        info!(
            "[ORCHESTRATOR] Scheduled {} tests into {} batches with {} sandboxes",
            tests_to_run.len(),
            scheduler.batch_count(),
            sandboxes.len()
        );
        for (i, size) in scheduler.batch_sizes().iter().enumerate() {
            info!("[ORCHESTRATOR] Batch {}: {} tests", i, size);
        }
        let total_in_batches: usize = scheduler.batch_sizes().iter().sum();
        info!(
            "[ORCHESTRATOR] Total tests across all batches: {} (should equal {})",
            total_in_batches,
            tests_to_run.len()
        );

        // Shared JUnit report for accumulating results and early stopping
        let total_tests_to_run = tests.len();
        let junit_report = Arc::new(std::sync::Mutex::new(MasterJunitReport::new(
            total_tests_to_run,
        )));
        let mut tracker = completion::CompletionTracker::new(total_tests_to_run);
        for test in tests {
            tracker.register_retries(&test.id, test.retry_count + 1);
        }
        let tracker = Arc::new(std::sync::Mutex::new(tracker));
        let all_complete = Arc::new(AtomicBool::new(false));
        let cancellation_token = CancellationToken::new();

        // Collect sandboxes back after use for termination
        let sandboxes_for_cleanup = Arc::new(std::sync::Mutex::new(Vec::new()));

        let batch_counter = Arc::new(AtomicUsize::new(0));
        let ci_last_decided = Arc::new(AtomicUsize::new(0));

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
                    scheduler: &scheduler,
                    progress: &progress,
                    total_tests_to_run,
                    all_complete: Arc::clone(&all_complete),
                    cancellation_token: cancellation_token.clone(),
                    sandboxes_for_cleanup: Arc::clone(&sandboxes_for_cleanup),
                    junit_report: Arc::clone(&junit_report),
                    logs_dir: logs_dir.clone(),
                    batch_counter: Arc::clone(&batch_counter),
                    verbose: self.verbose,
                    tracer: self.tracer.clone(),
                    sandbox_index,
                    fail_fast: self.fail_fast,
                    tracker: Arc::clone(&tracker),
                    ci: self.ci,
                    ci_last_decided: Arc::clone(&ci_last_decided),
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

        // Check for missing test IDs.
        // Discovery may produce duplicate IDs (e.g. vitest describe.each),
        // so compare unique IDs, not raw record count.
        let expected_unique_ids: usize = {
            let mut ids = std::collections::HashSet::new();
            for t in tests {
                ids.insert(&t.id);
            }
            ids.len()
        };
        if total_in_report < expected_unique_ids {
            error!(
                "[ORCHESTRATOR MISMATCH] Expected {} unique test IDs but only {} in report! {} MISSING!",
                expected_unique_ids,
                total_in_report,
                expected_unique_ids - total_in_report
            );
        } else {
            info!(
                "[ORCHESTRATOR] All {} expected test IDs accounted for in report",
                expected_unique_ids
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

        // Record results to history store
        let should_record = if let Some(ref history_cfg) = self.config.history {
            match history_cfg.record_history {
                RecordHistory::Always => true,
                RecordHistory::Flag => self.record_history_flag,
            }
        } else {
            false
        };
        if should_record && let Err(e) = self.record_history(&junit_report) {
            warn!("Failed to record test history: {}", e);
        }

        // Use JUnit report as source of truth for all counts
        let total_in_junit = if let Ok(report) = junit_report.lock() {
            report.total_count()
        } else {
            0
        };
        let not_run = expected_unique_ids.saturating_sub(total_in_junit);

        // Use the JUnit total as the authoritative count (passed + failed + flaky = total)
        // This ensures passed can never exceed total
        // Note: estimated_cost is set to default here and updated after sandbox cleanup
        let run_result = RunResult {
            total_tests: total_in_junit,
            passed: passed + flaky_count, // Flaky tests count as passed
            failed,
            flaky: flaky_count,
            not_run,
            duration: start.elapsed(),
            estimated_cost: CostEstimate::default(),
        };
        drop(_agg_span);

        progress.finish_and_clear();

        // Terminate all sandboxes in parallel (after printing results)
        // Aggregate cost estimates BEFORE terminating (cost_estimate uses elapsed time)
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

        // Aggregate cost estimates before terminating sandboxes
        let estimated_cost = sandboxes
            .iter()
            .fold(CostEstimate::default(), |mut acc, sb| {
                let cost = sb.cost_estimate();
                acc.cpu_seconds += cost.cpu_seconds;
                acc.estimated_cost_usd += cost.estimated_cost_usd;
                acc
            });

        let term_progress = if self.ci {
            eprintln!("Terminating {} sandboxes...", sandboxes.len());
            indicatif::ProgressBar::hidden()
        } else {
            let pb = indicatif::ProgressBar::new(sandboxes.len() as u64);
            if let Ok(style) = indicatif::ProgressStyle::default_bar()
                .template("{spinner:.green} Terminating sandboxes [{bar:40.cyan/blue}] {pos}/{len}")
            {
                pb.set_style(style.progress_chars("#>-"));
            }
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        };
        let term_progress_ref = &term_progress;
        let terminate_futures = sandboxes.into_iter().map(|sandbox| async move {
            let id = sandbox.id().to_string();
            match tokio::time::timeout(std::time::Duration::from_secs(30), sandbox.terminate())
                .await
            {
                Ok(Err(e)) => debug!("Failed to terminate sandbox {}: {}", id, e),
                Err(_) => debug!("Timeout terminating sandbox {}", id),
                Ok(Ok(())) => {}
            }
            term_progress_ref.inc(1);
        });
        futures::future::join_all(terminate_futures).await;
        term_progress.finish_and_clear();
        if self.ci {
            eprintln!("Sandboxes terminated.");
        }
        drop(_cleanup_span);

        // Update run_result with estimated_cost
        let run_result = RunResult {
            estimated_cost,
            ..run_result
        };

        print_summary(&run_result, self.show_cost);

        Ok(run_result)
    }

    /// Records test results to the history store.
    ///
    /// Extracts attempt results from the JUnit report and writes them to the
    /// configured history file. Each testcase becomes a separate attempt record.
    fn record_history(&self, junit_report: &SharedJunitReport) -> anyhow::Result<()> {
        let history_cfg = self
            .config
            .history
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("record_history called without history config"))?;

        let report = junit_report
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock junit report: {}", e))?;

        let mut history_store = JsonlHistoryStore::load(
            &history_cfg.path,
            history_cfg.reservoir_size,
            history_cfg.default_duration_secs,
        )?;

        let results = self.extract_attempt_results(&report);
        if results.is_empty() {
            debug!("No test results to record in history");
            return Ok(());
        }

        info!(
            "[HISTORY] Recording {} test attempt(s) to {}",
            results.len(),
            history_cfg.path.display()
        );

        history_store.record_results(&results)?;
        history_store.save()?;

        Ok(())
    }

    /// Extracts test attempt results from a JUnit report for history recording.
    ///
    /// Each `<testcase>` element becomes one `TestAttemptResult`. Retries produce
    /// separate entries, which is correct for the history store's per-attempt tracking.
    fn extract_attempt_results(&self, report: &MasterJunitReport) -> Vec<TestAttemptResult> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let test_id_format = self.config.framework.test_id_format();

        report
            .testsuites()
            .iter()
            .flat_map(|ts| &ts.testcases)
            .filter(|tc| !tc.skipped)
            .map(|tc| TestAttemptResult {
                config: self.config_filename.clone(),
                test_id: format_test_id(test_id_format, &tc.name, tc.classname.as_deref()),
                run_id: self.run_id.clone(),
                passed: tc.failure.is_none() && tc.error.is_none(),
                duration_secs: tc.time,
                timestamp_ms: now_ms,
            })
            .collect()
    }
}
