//! Queue-based spawn logic for parallel test execution.
//!
//! Instead of zipping sandboxes with batches 1:1, workers pull batches
//! from a shared queue so N sandboxes can process M batches (M >= N).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::ProgressBar;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::framework::TestFramework;
use crate::provider::{OutputLine, Sandbox};
use crate::report::MasterJunitReport;

use super::runner::{ArtifactConfig, BatchOutcome, OutputCallback, RunnerConfig, TestRunner};
use super::scheduler::Scheduler;

/// Configuration for a queue-based spawn worker.
///
/// Each worker loops pulling batches from the shared `queue` and
/// executing them with a [`TestRunner`]. Batch indices are assigned
/// atomically via `batch_counter`.
pub(crate) struct SpawnConfig<'a, F: TestFramework, S: Sandbox> {
    pub config: &'a Config,
    pub framework: &'a F,
    pub scheduler: &'a Scheduler<'a>,
    pub progress: &'a ProgressBar,
    pub total_tests_to_run: usize,
    pub all_passed: Arc<AtomicBool>,
    pub cancellation_token: CancellationToken,
    pub sandboxes_for_cleanup: Arc<Mutex<Vec<S>>>,
    pub junit_report: Arc<Mutex<MasterJunitReport>>,
    pub logs_dir: PathBuf,
    pub batch_counter: Arc<AtomicUsize>,
    pub verbose: bool,
    pub tracer: crate::trace::Tracer,
    pub sandbox_index: usize,
    pub fail_fast: bool,
}

/// Runs a worker that pulls batches from a shared queue until empty.
///
/// The worker assigns batch indices atomically, sets up per-batch log
/// files, and drives the [`TestRunner`] for each batch.
pub(crate) async fn spawn_task<'a, F: TestFramework, S: Sandbox>(
    cfg: SpawnConfig<'a, F, S>,
    mut sandbox: S,
) {
    loop {
        let Some(batch) = cfg.scheduler.pop() else {
            break;
        };

        let batch_idx = cfg.batch_counter.fetch_add(1, Ordering::SeqCst);

        // Early exit if all tests have already passed or fail-fast triggered
        if cfg.all_passed.load(Ordering::SeqCst) || cfg.cancellation_token.is_cancelled() {
            let test_ids: Vec<_> = batch.tests.iter().map(|t| t.id()).collect();
            info!(
                "EARLY STOP: Skipping batch {} ({} tests) - cancelled",
                batch_idx,
                batch.tests.len()
            );
            debug!("Skipped tests: {:?}", test_ids);

            for suffix in ["stdout", "stderr"] {
                let log_src = cfg.logs_dir.join(format!("batch-{}.{}", batch_idx, suffix));
                if log_src.exists() {
                    let log_dst = cfg
                        .logs_dir
                        .join(format!("batch-{}.{}.cancelled", batch_idx, suffix));
                    if let Err(e) = std::fs::rename(&log_src, &log_dst) {
                        warn!("Failed to rename batch log: {}", e);
                    }
                }
            }

            if let Ok(mut cleanups) = cfg.sandboxes_for_cleanup.lock() {
                cleanups.push(sandbox);
            } else {
                error!("sandbox cleanup mutex poisoned during early stop");
            }
            return;
        }

        // Skip batches where all tests have already passed
        if let Ok(report) = cfg.junit_report.lock()
            && batch.tests.iter().all(|t| report.has_test_passed(t.id()))
        {
            let test_ids: Vec<_> = batch.tests.iter().map(|t| t.id()).collect();
            info!(
                "SKIP: Batch {} ({} tests) all already passed, skipping",
                batch_idx,
                batch.tests.len()
            );
            debug!("Skipped tests: {:?}", test_ids);
            cfg.progress.inc(batch.tests.len() as u64);
            continue;
        }

        let sandbox_pid = crate::trace::sandbox_pid(cfg.sandbox_index);
        let _batch_span = cfg
            .tracer
            .span(
                &format!("batch_{}", batch_idx),
                "exec",
                sandbox_pid,
                crate::trace::TID_EXEC,
            )
            .with_args(serde_json::json!({
                "batch_index": batch_idx,
                "test_count": batch.tests.len(),
                "sandbox_index": cfg.sandbox_index,
            }));

        // Set up runner
        let parts_dir = cfg.config.report.output_dir.join("junit-parts");
        let runner_config = RunnerConfig {
            fail_fast: cfg.fail_fast,
            parts_dir: Some(parts_dir),
            junit_report: Some(Arc::clone(&cfg.junit_report)),
            cancellation_token: Some(cfg.cancellation_token.clone()),
            artifacts: ArtifactConfig {
                globs: cfg.config.report.download_globs.clone(),
                output_dir: cfg.config.report.output_dir.clone(),
            },
        };
        let mut runner = TestRunner::new(
            sandbox,
            cfg.framework,
            Duration::from_secs(cfg.config.offload.test_timeout_secs),
            cfg.tracer.clone(),
            sandbox_pid,
            batch_idx,
            runner_config,
        );

        // Per-runner log files (separate stdout and stderr)
        {
            let stdout_path = cfg.logs_dir.join(format!("batch-{}.stdout", batch_idx));
            let stderr_path = cfg.logs_dir.join(format!("batch-{}.stderr", batch_idx));
            match (
                std::fs::File::create(&stdout_path),
                std::fs::File::create(&stderr_path),
            ) {
                (Ok(mut stdout_file), Ok(mut stderr_file)) => {
                    let callback: OutputCallback = Box::new(move |_test_id, line| {
                        let (file, msg): (&mut std::fs::File, _) = match line {
                            OutputLine::Stdout(s) => (&mut stdout_file, format!("{}\n", s)),
                            OutputLine::Stderr(s) => (&mut stderr_file, format!("{}\n", s)),
                            OutputLine::ExitCode(_) => return,
                        };
                        if let Err(e) = file.write_all(msg.as_bytes()) {
                            warn!("Failed to write to batch log: {}", e);
                        }
                    });
                    runner.set_output_callback(callback);
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!("Failed to create batch log files: {}", e);
                }
            }
        }

        // Verbose logging
        if cfg.verbose {
            for test in &batch.tests {
                println!("Running: {}", test.id());
            }
        }

        // Run tests
        let stdout_src = cfg.logs_dir.join(format!("batch-{}.stdout", batch_idx));
        let stderr_src = cfg.logs_dir.join(format!("batch-{}.stderr", batch_idx));
        let outcome = cfg
            .scheduler
            .run_batch(&batch, runner.run_tests(&batch.tests))
            .await;

        // Rename log files based on outcome
        let extension = match &outcome {
            Ok(BatchOutcome::Success) => "success",
            Ok(BatchOutcome::Failure) => "failure",
            Ok(BatchOutcome::Cancelled) => "cancelled",
            Err(_) => "error",
        };
        for src in [&stdout_src, &stderr_src] {
            if src.exists() {
                let dst = cfg.logs_dir.join(format!(
                    "{}.{}",
                    src.file_name().unwrap_or_default().to_string_lossy(),
                    extension
                ));
                if let Err(e) = std::fs::rename(src, &dst) {
                    warn!("Failed to rename batch log: {}", e);
                }
            }
        }

        // Handle outcome
        match &outcome {
            Ok(BatchOutcome::Success) | Ok(BatchOutcome::Failure) => {
                // Early stop: all tests passed
                if let Ok(report) = cfg.junit_report.lock()
                    && report.all_passed()
                    && !cfg.all_passed.load(Ordering::SeqCst)
                {
                    info!(
                        "EARLY STOP TRIGGERED: All {} tests have passed after batch {} completed. Cancelling remaining batches.",
                        cfg.total_tests_to_run, batch_idx
                    );
                    cfg.all_passed.store(true, Ordering::SeqCst);
                    cfg.cancellation_token.cancel();
                }
                // Fail-fast: cancel on first failure
                if cfg.fail_fast
                    && matches!(&outcome, Ok(BatchOutcome::Failure))
                    && !cfg.cancellation_token.is_cancelled()
                {
                    info!(
                        "FAIL-FAST: Batch {} had failures. Cancelling remaining batches.",
                        batch_idx
                    );
                    cfg.cancellation_token.cancel();
                }
            }
            Ok(BatchOutcome::Cancelled) => {
                debug!("Batch {} was cancelled", batch_idx);
            }
            Err(e) => {
                error!("Batch execution error: {}", e);
            }
        }

        cfg.progress.inc(batch.tests.len() as u64);
        if let Ok(report) = cfg.junit_report.lock() {
            let passed = report.passed_count();
            let failed = report.failed_count();
            let flaky = report.flaky_count();
            let awaiting = cfg.total_tests_to_run.saturating_sub(report.total_count());
            cfg.progress.set_message(format!(
                "{}\n{}\n{}\n{}",
                console::style(format!("passed: {passed}")).green(),
                console::style(format!("failed: {failed}")).red(),
                console::style(format!("flaky: {flaky}")).yellow(),
                console::style(format!("awaiting: {awaiting}")).dim(),
            ));
        }
        drop(_batch_span);
        sandbox = runner.into_sandbox();
    }

    // Return sandbox for cleanup
    if let Ok(mut cleanups) = cfg.sandboxes_for_cleanup.lock() {
        cleanups.push(sandbox);
    } else {
        error!("sandbox cleanup mutex poisoned during cleanup");
    }
}
