//! Queue-based spawn logic for parallel test execution.
//!
//! Instead of zipping sandboxes with batches 1:1, workers pull batches
//! from a shared queue so N sandboxes can process M batches (M >= N).

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::ProgressBar;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::framework::{TestFramework, TestInstance};
use crate::provider::{OutputLine, Sandbox};
use crate::report::MasterJunitReport;

use super::runner::{BatchOutcome, OutputCallback, TestRunner};

/// Configuration for a queue-based spawn worker.
///
/// Each worker loops pulling batches from the shared `queue` and
/// executing them with a [`TestRunner`]. Batch indices are assigned
/// atomically via `batch_counter`.
pub(crate) struct SpawnConfig<'a, F: TestFramework, S: Sandbox> {
    pub config: &'a Config,
    pub framework: &'a F,
    pub queue: Arc<Mutex<VecDeque<Vec<TestInstance<'a>>>>>,
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
        let batch = match cfg.queue.lock() {
            Ok(mut q) => q.pop_front(),
            Err(e) => {
                error!("batch queue mutex poisoned: {}", e);
                break;
            }
        };
        let Some(batch) = batch else { break };

        let batch_idx = cfg.batch_counter.fetch_add(1, Ordering::SeqCst);

        // Early exit if all tests have already passed
        if cfg.all_passed.load(Ordering::SeqCst) {
            let test_ids: Vec<_> = batch.iter().map(|t| t.id()).collect();
            info!(
                "EARLY STOP: Skipping batch {} ({} tests) - all tests already passed",
                batch_idx,
                batch.len()
            );
            debug!("Skipped tests: {:?}", test_ids);

            // Rename log to .cancelled if it exists
            let log_src = cfg.logs_dir.join(format!("batch-{}.log", batch_idx));
            if log_src.exists() {
                let log_dst = cfg.logs_dir.join(format!("batch-{}.cancelled", batch_idx));
                if let Err(e) = std::fs::rename(&log_src, &log_dst) {
                    warn!("Failed to rename batch log: {}", e);
                }
            }

            if let Ok(mut cleanups) = cfg.sandboxes_for_cleanup.lock() {
                cleanups.push(sandbox);
            } else {
                error!("sandbox cleanup mutex poisoned during early stop");
            }
            return;
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
                "test_count": batch.len(),
                "sandbox_index": cfg.sandbox_index,
            }));

        // Set up runner
        let parts_dir = cfg.config.report.output_dir.join("junit-parts");
        let mut runner = TestRunner::new(
            sandbox,
            cfg.framework,
            Duration::from_secs(cfg.config.offload.test_timeout_secs),
            cfg.tracer.clone(),
            sandbox_pid,
        )
        .with_cancellation_token(cfg.cancellation_token.clone())
        .with_junit_report(Arc::clone(&cfg.junit_report))
        .with_parts_dir(parts_dir);

        // Per-runner log file
        {
            let log_path = cfg.logs_dir.join(format!("batch-{}.log", batch_idx));
            match std::fs::File::create(&log_path) {
                Ok(file) => {
                    let log_file = Arc::new(std::sync::Mutex::new(file));
                    let callback: OutputCallback = Arc::new(move |_test_id, line| {
                        let msg = match line {
                            OutputLine::Stdout(s) => format!("{}\n", s),
                            OutputLine::Stderr(s) => format!("[stderr] {}\n", s),
                            OutputLine::ExitCode(_) => return,
                        };
                        if let Ok(mut f) = log_file.lock()
                            && let Err(e) = f.write_all(msg.as_bytes())
                        {
                            warn!("Failed to write to batch log: {}", e);
                        }
                    });
                    runner = runner.with_output_callback(callback);
                }
                Err(e) => {
                    warn!("Failed to create batch log {}: {}", log_path.display(), e);
                }
            }
        }

        // Verbose logging
        if cfg.verbose {
            for test in &batch {
                println!("Running: {}", test.id());
            }
        }

        // Run tests
        let log_src = cfg.logs_dir.join(format!("batch-{}.log", batch_idx));
        let outcome = runner.run_tests(&batch).await;

        // Rename log file based on outcome
        let extension = match &outcome {
            Ok(BatchOutcome::Success) => "success",
            Ok(BatchOutcome::Failure) => "failure",
            Ok(BatchOutcome::Cancelled) => "cancelled",
            Err(_) => "error",
        };
        if log_src.exists() {
            let log_dst = cfg
                .logs_dir
                .join(format!("batch-{}.{}", batch_idx, extension));
            if let Err(e) = std::fs::rename(&log_src, &log_dst) {
                warn!("Failed to rename batch log: {}", e);
            }
        }

        // Handle outcome
        match &outcome {
            Ok(BatchOutcome::Success) | Ok(BatchOutcome::Failure) => {
                if cfg.framework.supports_early_stopping()
                    && let Ok(report) = cfg.junit_report.lock()
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
            }
            Ok(BatchOutcome::Cancelled) => {
                debug!("Batch {} was cancelled", batch_idx);
            }
            Err(e) => {
                error!("Batch execution error: {}", e);
            }
        }

        cfg.progress.inc(batch.len() as u64);
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
