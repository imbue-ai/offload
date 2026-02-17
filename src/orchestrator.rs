//! Test execution engine and orchestration.
//!
//! This module contains the core execution logic that coordinates test
//! discovery, distribution across sandboxes, execution, retry handling,
//! and result collection.
//!
//! # Architecture
//!
//! ```text
//!   Framework                 Scheduler                Provider
//!       │                         │                        │
//!       │ discover()              │                        │
//!       ▼                         │                        │
//!  Vec<TestRecord>                │                        │
//!       │                         │                        │
//!       │ expand to TestInstances │                        │
//!       ▼                         │                        │
//!  Vec<TestInstance> ────────────►│ schedule_random()      │
//!                                 ▼                        │
//!                        Vec<Vec<TestInstance>> (batches)  │
//!                                 │                        │
//!                                 │    create_sandbox() ──►│
//!                                 │                        ▼
//!                                 │                     Sandbox
//!                                 │                        │
//!                                 └────────┬───────────────┘
//!                                          ▼
//!                                     TestRunner
//!                                          │
//!   Framework ◄─── produce_command() ──────┤
//!       │                                  │
//!       │                        Sandbox.exec(cmd)
//!       │                                  │
//!       │ parse_results() ◄─────── ExecResult
//!       ▼
//!  Vec<TestResult> ──► TestRecord.record_result()
//! ```
//!
//! # Execution Flow
//!
//! 1. **Discovery**: Find tests using the configured framework
//! 2. **Expansion**: Create parallel retry instances for each test
//! 3. **Scheduling**: Distribute test instances into batches across sandboxes
//! 4. **Execution**: Run test batches in parallel sandboxes
//! 5. **Aggregation**: Combine results (any pass = pass, detect flaky tests)
//! 6. **Reporting**: Print summary and generate JUnit XML
//!
//! # Key Components
//!
//! - [`Orchestrator`]: Main entry point coordinating the test run
//! - [`Scheduler`]: Distributes tests across available sandboxes
//! - [`TestRunner`]: Executes tests in a single sandbox
//! - [`RunResult`]: Aggregated results of the entire test run
//!
//! # Example
//!
//! ```no_run
//! use tokio::sync::Mutex;
//! use offload::orchestrator::{Orchestrator, SandboxPool};
//! use offload::config::load_config;
//! use offload::provider::local::LocalProvider;
//! use offload::framework::{TestFramework, pytest::PytestFramework};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = load_config(std::path::Path::new("offload.toml"))?;
//!
//!     let provider = LocalProvider::new(Default::default());
//!     let framework = PytestFramework::new(Default::default());
//!
//!     // Discover tests using the framework
//!     let tests = framework.discover(&[]).await?;
//!
//!     // Run tests using the orchestrator
//!     let orchestrator = Orchestrator::new(config, provider, framework, &[], false);
//!     let sandbox_pool = Mutex::new(SandboxPool::new());
//!     let result = orchestrator.run_with_tests(&tests, &sandbox_pool).await?;
//!
//!     if result.success() {
//!         println!("All tests passed!");
//!     } else {
//!         println!("{} tests failed", result.failed);
//!     }
//!
//!     std::process::exit(result.exit_code());
//! }
//! ```

pub mod pool;
pub mod runner;
pub mod scheduler;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::config::{Config, SandboxConfig};
use crate::framework::{TestFramework, TestInstance, TestRecord, TestResult};
use crate::provider::{OutputLine, SandboxProvider};
use crate::report::{MasterJunitReport, print_summary};

pub use pool::SandboxPool;
pub use runner::{OutputCallback, TestRunner};
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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::RunResult;
    /// use std::time::Duration;
    ///
    /// let result = RunResult {
    ///     total_tests: 100,
    ///     passed: 95,
    ///     failed: 0,
    ///     skipped: 5,
    ///     flaky: 2,
    ///     not_run: 0,
    ///     duration: Duration::from_secs(60),
    ///     results: vec![],
    /// };
    ///
    /// assert!(result.success());
    /// ```
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
/// - A [`SandboxProvider`] for execution environments
/// - A [`TestFramework`] for finding tests
///
/// It manages the full lifecycle of a test run: discovery, scheduling,
/// parallel execution, retries, and result aggregation.
///
/// # Type Parameters
///
/// - `P`: The sandbox provider type
/// - `D`: The test framework type
///
/// # Example
///
/// ```no_run
/// use tokio::sync::Mutex;
/// use offload::orchestrator::{Orchestrator, SandboxPool};
/// use offload::config::load_config;
/// use offload::provider::local::LocalProvider;
/// use offload::framework::{TestFramework, pytest::PytestFramework};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let config = load_config(std::path::Path::new("offload.toml"))?;
///
///     // Set up components
///     let provider = LocalProvider::new(Default::default());
///     let framework = PytestFramework::new(Default::default());
///
///     // Discover tests using the framework
///     let tests = framework.discover(&[]).await?;
///
///     // Create orchestrator and run tests
///     let orchestrator = Orchestrator::new(config, provider, framework, &[], false);
///     let sandbox_pool = Mutex::new(SandboxPool::new());
///     let result = orchestrator.run_with_tests(&tests, &sandbox_pool).await?;
///
///     std::process::exit(result.exit_code());
/// }
/// ```
pub struct Orchestrator<P, D> {
    config: Config,
    provider: P,
    framework: D,
    copy_dirs: Vec<(std::path::PathBuf, std::path::PathBuf)>,
    verbose: bool,
}

impl<P, D> Orchestrator<P, D>
where
    P: SandboxProvider,
    D: TestFramework,
{
    /// Creates a new orchestrator with the given components.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration loaded from TOML
    /// * `provider` - Sandbox provider for creating execution environments
    /// * `framework` - Test framework for running tests
    /// * `copy_dirs` - Directories to copy to sandboxes (local_path, remote_path)
    /// * `verbose` - Whether to show verbose output (streaming test output)
    pub fn new(
        config: Config,
        provider: P,
        framework: D,
        copy_dirs: &[(std::path::PathBuf, std::path::PathBuf)],
        verbose: bool,
    ) -> Self {
        Self {
            config,
            provider,
            framework,
            copy_dirs: copy_dirs.to_vec(),
            verbose,
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
        sandbox_pool: &Mutex<SandboxPool<P::Sandbox>>,
    ) -> anyhow::Result<RunResult> {
        let start = std::time::Instant::now();

        // Clear output directory to avoid stale results
        let output_dir = &self.config.report.output_dir;
        if output_dir.exists() {
            std::fs::remove_dir_all(output_dir).ok();
        }
        std::fs::create_dir_all(output_dir).ok();

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

        // Schedule tests into batches using random distribution
        let scheduler = Scheduler::new(self.config.offload.max_parallel);
        let batches = scheduler.schedule(&tests_to_run);

        debug!(
            "Scheduled {} tests into {} batches",
            tests_to_run.len(),
            batches.len()
        );

        // Shared JUnit report for accumulating results and early stopping
        let total_tests_to_run = tests.iter().filter(|t| !t.skipped).count();
        let junit_report = Arc::new(std::sync::Mutex::new(MasterJunitReport::new(
            total_tests_to_run,
        )));
        let all_passed = Arc::new(AtomicBool::new(false));
        let cancellation_token = CancellationToken::new();

        // Run tests in parallel
        // Execute batches concurrently using scoped spawns (no 'static required)
        tokio_scoped::scope(|scope| {
            for (batch_idx, batch) in batches.into_iter().enumerate() {
                let provider = &self.provider;
                let framework = &self.framework;
                let config = &self.config;
                let progress = &progress;
                let verbose = self.verbose;
                let junit_report = Arc::clone(&junit_report);
                let all_passed = Arc::clone(&all_passed);
                let cancellation_token = cancellation_token.clone();

                scope.spawn(async move {
                    // Early exit if all tests have already passed
                    if all_passed.load(Ordering::SeqCst) {
                        debug!("Batch {} skipped - all tests have passed", batch_idx);
                        return;
                    }
                    // Take sandbox from pool or create new one
                    let sandbox = {
                        let existing = sandbox_pool.lock().await.take_one();
                        if let Some(s) = existing {
                            s
                        } else {
                            let sandbox_config = SandboxConfig {
                                id: format!("offload-{}-{}", uuid::Uuid::new_v4(), batch_idx),
                                working_dir: config
                                    .offload
                                    .working_dir
                                    .as_ref()
                                    .map(|p| p.to_string_lossy().to_string()),
                                env: Vec::new(),
                                copy_dirs: self.copy_dirs.clone(),
                            };
                            match provider.create_sandbox(&sandbox_config).await {
                                Ok(s) => s,
                                Err(e) => {
                                    error!("Failed to create sandbox: {}", e);
                                    return;
                                }
                            }
                        }
                    };

                    let mut runner = TestRunner::new(
                        sandbox,
                        framework,
                        Duration::from_secs(config.offload.test_timeout_secs),
                    )
                    .with_cancellation_token(cancellation_token.clone())
                    .with_junit_report(Arc::clone(&junit_report));

                    // Enable output callback only in verbose mode
                    if config.offload.stream_output && self.verbose {
                        let callback: OutputCallback = Arc::new(|test_id, line| match line {
                            OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
                            OutputLine::Stderr(s) => eprintln!("[{}] {}", test_id, s),
                            OutputLine::ExitCode(_) => {}
                        });
                        runner = runner.with_output_callback(callback);
                    }

                    // Log test starts in verbose mode
                    if verbose {
                        for test in &batch {
                            println!("Running: {}", test.id());
                        }
                    }

                    // Run all tests in batch with a single command
                    match runner.run_tests(&batch).await {
                        Ok(true) => {
                            // Check shared report for early stopping
                            if let Ok(report) = junit_report.lock()
                                && report.all_passed()
                                && !all_passed.load(Ordering::SeqCst)
                            {
                                debug!(
                                    "All {} tests have passed, signaling early stop",
                                    total_tests_to_run
                                );
                                all_passed.store(true, Ordering::SeqCst);
                                cancellation_token.cancel();
                            }
                        }
                        Ok(false) => {
                            // Batch was cancelled - no results to record
                            debug!("Batch {} was cancelled", batch_idx);
                        }
                        Err(e) => {
                            error!("Batch execution error: {}", e);
                        }
                    }

                    // Update progress for completed batch
                    progress.inc(batch.len() as u64);

                    // Return sandbox to pool for reuse (don't terminate)
                    let sandbox = runner.into_sandbox();
                    sandbox_pool.lock().await.add(sandbox);
                });
            }
        });

        // Aggregate results from TestRecords (handles parallel retries automatically)
        // Get results from the shared JUnit report
        let (passed, failed, flaky_count) = if let Ok(report) = junit_report.lock() {
            report.summary()
        } else {
            (0, 0, 0)
        };

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

        // Total tests from discovery, results from JUnit XML
        let total_discovered = tests.iter().filter(|t| !t.skipped).count();
        let tests_with_results = if let Ok(report) = junit_report.lock() {
            report.total_count()
        } else {
            0
        };
        let not_run = total_discovered.saturating_sub(tests_with_results);

        let run_result = RunResult {
            total_tests: total_discovered,
            passed: passed + flaky_count, // Flaky tests count as passed
            failed,
            skipped: skipped_count,
            flaky: flaky_count,
            not_run,
            duration: start.elapsed(),
            results: Vec::new(), // Results are in JUnit XML now
        };

        progress.finish_and_clear();
        print_summary(&run_result);

        Ok(run_result)
    }
}
