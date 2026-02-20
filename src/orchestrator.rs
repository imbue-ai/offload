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
//! use offload::orchestrator::{Orchestrator, SandboxPool};
//! use offload::config::{load_config, SandboxConfig};
//! use offload::provider::local::LocalProvider;
//! use offload::framework::{TestFramework, pytest::PytestFramework};
//! use offload::report::JunitFormat;
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
//!     // Pre-populate sandbox pool
//!     let sandbox_config = SandboxConfig {
//!         id: "sandbox".to_string(),
//!         working_dir: None,
//!         env: vec![],
//!         copy_dirs: vec![],
//!     };
//!     let mut sandbox_pool = SandboxPool::new();
//!     sandbox_pool.populate(config.offload.max_parallel, &provider, &sandbox_config).await?;
//!
//!     // Run tests using the orchestrator
//!     let orchestrator = Orchestrator::new(config, framework, false, JunitFormat::Pytest);
//!     let result = orchestrator.run_with_tests(&tests, sandbox_pool).await?;
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

use crate::config::Config;
use crate::framework::{TestFramework, TestInstance, TestRecord, TestResult};
use crate::provider::{OutputLine, Sandbox};
use crate::report::{MasterJunitReport, load_test_durations, print_summary};

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
/// # Example
///
/// ```no_run
/// use offload::orchestrator::{Orchestrator, SandboxPool};
/// use offload::config::{load_config, SandboxConfig};
/// use offload::provider::local::LocalProvider;
/// use offload::framework::{TestFramework, pytest::PytestFramework};
/// use offload::report::JunitFormat;
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
///     // Pre-populate sandbox pool
///     let sandbox_config = SandboxConfig {
///         id: "sandbox".to_string(),
///         working_dir: None,
///         env: vec![],
///         copy_dirs: vec![],
///     };
///     let mut sandbox_pool = SandboxPool::new();
///     sandbox_pool.populate(config.offload.max_parallel, &provider, &sandbox_config).await?;
///
///     // Create orchestrator and run tests
///     let orchestrator = Orchestrator::new(config, framework, false, JunitFormat::Pytest);
///     let result = orchestrator.run_with_tests(&tests, sandbox_pool).await?;
///
///     std::process::exit(result.exit_code());
/// }
/// ```
pub struct Orchestrator<S, D> {
    config: Config,
    framework: D,
    verbose: bool,
    junit_format: crate::report::JunitFormat,
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
    /// * `framework` - Test framework for running tests
    /// * `verbose` - Whether to show verbose output (streaming test output)
    /// * `junit_format` - Format for parsing test IDs from JUnit XML
    pub fn new(
        config: Config,
        framework: D,
        verbose: bool,
        junit_format: crate::report::JunitFormat,
    ) -> Self {
        Self {
            config,
            framework,
            verbose,
            junit_format,
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
        let junit_path = self
            .config
            .report
            .output_dir
            .join(&self.config.report.junit_file);
        let durations = load_test_durations(&junit_path, self.junit_format);

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
        // otherwise fall back to round-robin with a warning and user confirmation.
        let scheduler = Scheduler::new(self.config.offload.max_parallel);
        let batches = if durations.is_empty() {
            warn!(
                "No historical test durations found at {}. Falling back to round-robin scheduling. \
                 Run tests once to generate junit.xml for optimized LPT scheduling.",
                junit_path.display()
            );
            eprintln!();
            eprintln!("WARNING: No junit.xml found at {}", junit_path.display());
            eprintln!(
                "Using round-robin scheduling instead of LPT (suboptimal for parallel execution)."
            );
            eprintln!("Run tests once to generate junit.xml for optimized scheduling.");
            eprintln!();
            eprint!("Press Enter to continue with round-robin scheduling...");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut input = String::new();
            let _ = std::io::stdin().read_line(&mut input);
            scheduler.schedule(&tests_to_run)
        } else {
            debug!(
                "Using LPT scheduling with {} historical durations from {}",
                durations.len(),
                junit_path.display()
            );
            // Default duration for unknown tests: 1 second (conservative estimate)
            scheduler.schedule_lpt(&tests_to_run, &durations, std::time::Duration::from_secs(1))
        };

        // Take sandboxes from pool - must match batch count
        let sandboxes = sandbox_pool.take_all();
        assert_eq!(
            sandboxes.len(),
            batches.len(),
            "sandbox count ({}) must match batch count ({})",
            sandboxes.len(),
            batches.len()
        );

        debug!(
            "Scheduled {} tests into {} batches with {} sandboxes",
            tests_to_run.len(),
            batches.len(),
            sandboxes.len()
        );

        // Shared JUnit report for accumulating results and early stopping
        let total_tests_to_run = tests.iter().filter(|t| !t.skipped).count();
        let junit_report = Arc::new(std::sync::Mutex::new(MasterJunitReport::new(
            total_tests_to_run,
        )));
        let all_passed = Arc::new(AtomicBool::new(false));
        let cancellation_token = CancellationToken::new();

        // Collect sandboxes back after use for termination
        let sandboxes_for_cleanup = Arc::new(Mutex::new(Vec::new()));

        // Run tests in parallel
        // Execute batches concurrently using scoped spawns (no 'static required)
        tokio_scoped::scope(|scope| {
            for (batch_idx, (sandbox, batch)) in sandboxes.into_iter().zip(batches).enumerate() {
                let framework = &self.framework;
                let config = &self.config;
                let progress = &progress;
                let verbose = self.verbose;
                let junit_report = Arc::clone(&junit_report);
                let all_passed = Arc::clone(&all_passed);
                let cancellation_token = cancellation_token.clone();
                let sandboxes_for_cleanup = Arc::clone(&sandboxes_for_cleanup);

                scope.spawn(async move {
                    // Early exit if all tests have already passed
                    if all_passed.load(Ordering::SeqCst) {
                        debug!("Batch {} skipped - all tests have passed", batch_idx);
                        sandboxes_for_cleanup.lock().await.push(sandbox);
                        return;
                    }

                    let parts_dir = config.report.output_dir.join("junit-parts");
                    let mut runner = TestRunner::new(
                        sandbox,
                        framework,
                        Duration::from_secs(config.offload.test_timeout_secs),
                    )
                    .with_cancellation_token(cancellation_token.clone())
                    .with_junit_report(Arc::clone(&junit_report))
                    .with_parts_dir(parts_dir);

                    // Enable output callback only in verbose mode
                    if config.offload.stream_output && verbose {
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

                    // Collect sandbox for cleanup
                    let sandbox = runner.into_sandbox();
                    sandboxes_for_cleanup.lock().await.push(sandbox);
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

        // Terminate all sandboxes in parallel (after printing results)
        let sandboxes: Vec<_> = sandboxes_for_cleanup.lock().await.drain(..).collect();
        let terminate_futures = sandboxes.into_iter().map(|sandbox| async move {
            if let Err(e) = sandbox.terminate().await {
                warn!("Failed to terminate sandbox {}: {}", sandbox.id(), e);
            }
        });
        futures::future::join_all(terminate_futures).await;

        Ok(run_result)
    }
}
