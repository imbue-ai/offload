//! Queue-based spawn logic for parallel test execution.
//!
//! Instead of zipping sandboxes with batches 1:1, workers pull batches
//! from a shared queue so N sandboxes can process M batches (M >= N).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use parking_lot::RwLock;
use std::time::Duration;

use indicatif::ProgressBar;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::framework::TestFramework;
use crate::provider::{OutputLine, Sandbox};
use crate::report::MasterJunitReport;

use super::completion::CompletionTracker;
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
    pub scheduler: &'a Scheduler,
    pub progress: &'a ProgressBar,
    pub total_tests_to_run: usize,
    pub cancellation_token: CancellationToken,
    pub sandboxes_for_cleanup: Arc<Mutex<Vec<S>>>,
    pub junit_report: Arc<Mutex<MasterJunitReport>>,
    pub logs_dir: PathBuf,
    pub batch_counter: Arc<AtomicUsize>,
    pub verbose: bool,
    pub tracer: crate::trace::Tracer,
    pub sandbox_index: usize,
    pub fail_fast: bool,
    pub tracker: Arc<RwLock<CompletionTracker>>,
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
        let batch = tokio::select! {
            batch = cfg.scheduler.pop() => batch,
            () = cfg.cancellation_token.cancelled() => None,
        };
        let Some(batch) = batch else {
            break;
        };

        let batch_idx = cfg.batch_counter.fetch_add(1, Ordering::SeqCst);
        info!(
            "Worker {} picked up batch {} ({} tests)",
            cfg.sandbox_index,
            batch_idx,
            batch.tests.len()
        );

        // Early exit if cancelled (fail-fast or all tests decided)
        if cfg.cancellation_token.is_cancelled() {
            info!(
                "EARLY STOP: Skipping batch {} ({} tests) - cancelled",
                batch_idx,
                batch.tests.len()
            );
            if let Ok(mut cleanups) = cfg.sandboxes_for_cleanup.lock() {
                cleanups.push(sandbox);
            } else {
                error!("sandbox cleanup mutex poisoned during early stop");
            }
            return;
        }

        // Skip if all tests already decided (read lock — concurrent with other workers)
        if cfg
            .tracker
            .read()
            .all_decided_by_name(batch.tests.iter().map(|t| t.id()))
        {
            info!(
                "SKIP: Batch {} ({} tests) all already decided",
                batch_idx,
                batch.tests.len()
            );
            continue;
        }

        // Register per-batch cancellation token (write lock)
        let child_token = cfg.cancellation_token.child_token();
        let test_ids: Vec<_> = batch.tests.iter().map(|t| t.id()).collect();
        cfg.tracker
            .write()
            .register_batch(batch_idx, &test_ids, child_token.clone());

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
            parts_dir,
            junit_report: Arc::clone(&cfg.junit_report),
            tracker: Arc::clone(&cfg.tracker),
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

        let stdout_src = cfg.logs_dir.join(format!("batch-{}.stdout", batch_idx));
        let stderr_src = cfg.logs_dir.join(format!("batch-{}.stderr", batch_idx));
        let outcome = {
            let exec_fut = cfg
                .scheduler
                .register_running_batch(&batch, runner.run_tests(&batch.tests));
            tokio::pin!(exec_fut);

            // First await: race against per-batch child token
            let first = tokio::select! {
                result = &mut exec_fut => Some(result),
                () = child_token.cancelled() => None,
            };

            match first {
                Some(result) => result,
                None => {
                    // Per-batch token fired — tests decided by other batches.
                    // TODO: Return BatchOutcome::Cancelled here once validated,
                    // to free the sandbox immediately.
                    // Second await: still race against global token so we don't
                    // block on a batch that will never finish.
                    tokio::select! {
                        result = exec_fut => result,
                        () = cfg.cancellation_token.cancelled() => Ok(BatchOutcome::Cancelled),
                    }
                }
            }
        };

        info!(
            "Worker {} finished batch {} ({} tests): {:?}",
            cfg.sandbox_index,
            batch_idx,
            batch.tests.len(),
            outcome
                .as_ref()
                .map(|o| *o)
                .unwrap_or(BatchOutcome::Cancelled)
        );

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

        // Fail-fast: cancel all on first failure
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
        if let Err(e) = &outcome {
            error!("Batch execution error: {}", e);
        }

        // Update progress from tracker (newly_complete_tests already called inside runner)
        if let Ok(report) = cfg.junit_report.lock() {
            let tracker = cfg.tracker.read();
            let decided = tracker.decided_count();
            cfg.progress.set_position(decided as u64);
            let passed = report.passed_count();
            let failed = report.failed_count();
            let flaky = report.flaky_count();
            let awaiting = cfg.total_tests_to_run - decided;
            cfg.progress.set_message(format!(
                "{}\n{}\n{}\n{}",
                console::style(format!("passed: {passed}")).green(),
                console::style(format!("failed: {failed}")).red(),
                console::style(format!("flaky: {flaky}")).yellow(),
                console::style(format!("awaiting: {awaiting}")).dim(),
            ));

            // Early stop: all tests decided — cancel remaining batches
            if tracker.all_complete() && !cfg.cancellation_token.is_cancelled() {
                info!(
                    "All {} tests decided after batch {}. Cancelling remaining batches.",
                    cfg.total_tests_to_run, batch_idx
                );
                cfg.cancellation_token.cancel();
            }
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
