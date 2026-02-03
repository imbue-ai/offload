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
//!                                          │
//!                                          ▼
//!                                      Reporter
//! ```
//!
//! # Execution Flow
//!
//! 1. **Discovery**: Find tests using the configured framework
//! 2. **Expansion**: Create parallel retry instances for each test
//! 3. **Scheduling**: Distribute test instances into batches across sandboxes
//! 4. **Execution**: Run test batches in parallel sandboxes
//! 5. **Aggregation**: Combine results (any pass = pass, detect flaky tests)
//! 6. **Reporting**: Notify reporters with final results
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
//! use offload::report::ConsoleReporter;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = load_config(std::path::Path::new("offload.toml"))?;
//!
//!     let provider = LocalProvider::new(Default::default());
//!     let framework = PytestFramework::new(Default::default());
//!     let reporter = ConsoleReporter::new(true);
//!
//!     // Discover tests using the framework
//!     let tests = framework.discover(&[]).await?;
//!
//!     // Run tests using the orchestrator
//!     let orchestrator = Orchestrator::new(config, provider, framework, reporter, &[]);
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
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::{Config, SandboxConfig, SandboxResources};
use crate::framework::{TestFramework, TestInstance, TestOutcome, TestRecord, TestResult};
use crate::provider::{OutputLine, SandboxProvider};
use crate::report::Reporter;

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
/// - A [`Reporter`] for output
///
/// It manages the full lifecycle of a test run: discovery, scheduling,
/// parallel execution, retries, and result aggregation.
///
/// # Type Parameters
///
/// - `P`: The sandbox provider type
/// - `D`: The test framework type
/// - `R`: The reporter type
///
/// # Example
///
/// ```no_run
/// use tokio::sync::Mutex;
/// use offload::orchestrator::{Orchestrator, SandboxPool};
/// use offload::config::load_config;
/// use offload::provider::local::LocalProvider;
/// use offload::framework::{TestFramework, pytest::PytestFramework};
/// use offload::report::ConsoleReporter;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let config = load_config(std::path::Path::new("offload.toml"))?;
///
///     // Set up components
///     let provider = LocalProvider::new(Default::default());
///     let framework = PytestFramework::new(Default::default());
///     let reporter = ConsoleReporter::new(true);
///
///     // Discover tests using the framework
///     let tests = framework.discover(&[]).await?;
///
///     // Create orchestrator and run tests
///     let orchestrator = Orchestrator::new(config, provider, framework, reporter, &[]);
///     let sandbox_pool = Mutex::new(SandboxPool::new());
///     let result = orchestrator.run_with_tests(&tests, &sandbox_pool).await?;
///
///     std::process::exit(result.exit_code());
/// }
/// ```
pub struct Orchestrator<P, D, R> {
    config: Config,
    provider: P,
    framework: D,
    reporter: R,
    copy_dirs: Vec<(std::path::PathBuf, std::path::PathBuf)>,
}

impl<P, D, R> Orchestrator<P, D, R>
where
    P: SandboxProvider,
    D: TestFramework,
    R: Reporter,
{
    /// Creates a new orchestrator with the given components.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration loaded from TOML
    /// * `provider` - Sandbox provider for creating execution environments
    /// * `framework` - Test framework for running tests
    /// * `reporter` - Reporter for outputting results
    /// * `copy_dirs` - Directories to copy to sandboxes (local_path, remote_path)
    pub fn new(
        config: Config,
        provider: P,
        framework: D,
        reporter: R,
        copy_dirs: &[(std::path::PathBuf, std::path::PathBuf)],
    ) -> Self {
        Self {
            config,
            provider,
            framework,
            reporter,
            copy_dirs: copy_dirs.to_vec(),
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

        self.reporter.on_discovery_complete(tests).await;

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
        let batches = scheduler.schedule_random(&tests_to_run);

        info!(
            "Scheduled {} tests into {} batches",
            tests_to_run.len(),
            batches.len()
        );

        // Run tests in parallel
        // Track which tests have been reported (for progress bar with parallel retries)
        let reported_tests: Mutex<std::collections::HashSet<String>> =
            Mutex::new(std::collections::HashSet::new());

        // Execute batches concurrently using scoped spawns (no 'static required)
        tokio_scoped::scope(|scope| {
            for (batch_idx, batch) in batches.into_iter().enumerate() {
                let provider = &self.provider;
                let framework = &self.framework;
                let reporter = &self.reporter;
                let config = &self.config;
                let reported_tests = &reported_tests;

                scope.spawn(async move {
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
                                resources: SandboxResources {
                                    timeout_secs: Some(config.offload.test_timeout_secs),
                                },
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
                    );

                    // Enable output callback if streaming is configured
                    if config.offload.stream_output {
                        let callback: OutputCallback = Arc::new(|test_id, line| match line {
                            OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
                            OutputLine::Stderr(s) => eprintln!("[{}] {}", test_id, s),
                        });
                        runner = runner.with_output_callback(callback);
                    }

                    // Report all tests as starting
                    for test in &batch {
                        reporter.on_test_start(test.record()).await;
                    }

                    // Run all tests in batch with a single command
                    match runner.run_tests(&batch).await {
                        Ok(()) => {
                            // Results are stored in each TestRecord
                        }
                        Err(e) => {
                            error!("Batch execution error: {}", e);
                            // Mark all tests in batch as errors
                            for test in &batch {
                                let failed_result = TestResult {
                                    test_id: test.id_owned(),
                                    outcome: TestOutcome::Error,
                                    duration: Duration::ZERO,
                                    stdout: String::new(),
                                    stderr: e.to_string(),
                                    error_message: Some(e.to_string()),
                                    stack_trace: None,
                                };
                                test.record_result(failed_result);
                            }
                        }
                    }

                    // Report test completions (only first instance of each test for progress bar)
                    for test in &batch {
                        let test_id = test.id_owned();
                        let already_reported = {
                            let mut reported = reported_tests.lock().await;
                            !reported.insert(test_id.clone())
                        };
                        if !already_reported && let Some(result) = test.record().final_result() {
                            reporter.on_test_complete(&result).await;
                        }
                    }

                    // Return sandbox to pool for reuse (don't terminate)
                    let sandbox = runner.into_sandbox();
                    sandbox_pool.lock().await.add(sandbox);
                });
            }
        });

        // Aggregate results from TestRecords (handles parallel retries automatically)
        // Each TestRecord may have multiple results; final_result() picks pass if any passed
        let all_results: Vec<TestResult> = tests
            .iter()
            .filter(|t| !t.skipped)
            .filter_map(|t| t.final_result())
            .collect();

        // Count flaky tests (passed but had failures)
        let flaky_count = tests
            .iter()
            .filter(|t| !t.skipped)
            .filter(|t| t.is_flaky())
            .count();

        // Calculate statistics from TestRecords (not aggregated results)
        // passed = number of TestRecords where at least one attempt passed
        let passed = tests
            .iter()
            .filter(|t| !t.skipped)
            .filter(|t| t.passed())
            .count();
        // failed = number of TestRecords where final outcome is failed/error (no passing attempts)
        let failed = tests
            .iter()
            .filter(|t| !t.skipped)
            .filter(|t| {
                t.final_result()
                    .map(|r| r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error)
                    .unwrap_or(false)
            })
            .count();
        let runtime_skipped = all_results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Skipped)
            .count();
        let unique_tests = tests.iter().filter(|t| !t.skipped).count();
        let not_run = unique_tests.saturating_sub(all_results.len());

        let run_result = RunResult {
            total_tests: tests.len(),
            passed,
            failed,
            skipped: skipped_count + runtime_skipped,
            flaky: flaky_count,
            not_run,
            duration: start.elapsed(),
            results: all_results,
        };

        self.reporter.on_run_complete(&run_result).await;

        Ok(run_result)
    }
}
