//! Test execution engine and orchestration.
//!
//! This module contains the core execution logic that coordinates test
//! discovery, distribution across sandboxes, execution, retry handling,
//! and result collection.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                         Orchestrator                                 │
//! │  (coordinates the entire test run)                                  │
//! │                                                                      │
//! │  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐            │
//! │  │  Framework  │   │   Scheduler  │   │   Reporter   │            │
//! │  │   (finds     │   │ (distributes │   │   (reports   │            │
//! │  │    tests)    │   │    tests)    │   │   results)   │            │
//! │  └──────┬───────┘   └──────┬───────┘   └──────┬───────┘            │
//! │         │                  │                  │                     │
//! │         ▼                  ▼                  │                     │
//! │  ┌────────────────────────────────────────────────────────────┐    │
//! │  │                      TestRunner (per sandbox)              │    │
//! │  │  ┌────────────┐                        ┌──────────────┐   │    │
//! │  │  │  Sandbox   │ ◄──── exec() ────────► │ RetryManager │   │    │
//! │  │  │ (provider) │                        │  (retries)   │   │    │
//! │  │  └────────────┘                        └──────────────┘   │    │
//! │  └────────────────────────────────────────────────────────────┘    │
//! │                                                                      │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Execution Flow
//!
//! 1. **Discovery**: Find tests using the configured framework
//! 2. **Scheduling**: Distribute tests into batches across sandboxes
//! 3. **Execution**: Run test batches in parallel sandboxes
//! 4. **Retry**: Re-run failed tests (if configured)
//! 5. **Reporting**: Aggregate results and notify reporters
//!
//! # Key Components
//!
//! - [`Orchestrator`]: Main entry point coordinating the test run
//! - [`Scheduler`]: Distributes tests across available sandboxes
//! - [`TestRunner`]: Executes tests in a single sandbox
//! - [`RetryManager`]: Handles retry logic and flaky test detection
//! - [`RunResult`]: Aggregated results of the entire test run
//!
//! # Example
//!
//! ```no_run
//! use tokio::sync::Mutex;
//! use offload::orchestrator::{Orchestrator, SandboxPool};
//! use offload::config::load_config;
//! use offload::provider::local::LocalProvider;
//! use offload::framework::pytest::PytestFramework;
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
//!     let orchestrator = Orchestrator::new(config, "example".to_string(), provider, framework, reporter);
//!     let sandbox_pool = Mutex::new(SandboxPool::new());
//!     let result = orchestrator.run(&sandbox_pool).await?;
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
pub mod retry;
pub mod runner;
pub mod scheduler;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::{Config, SandboxConfig, SandboxResources};
use crate::framework::{TestFramework, TestInstance, TestOutcome, TestResult};
use crate::provider::{OutputLine, SandboxProvider};
use crate::report::Reporter;

pub use pool::SandboxPool;
pub use retry::RetryManager;
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
/// use offload::framework::pytest::PytestFramework;
/// use offload::report::{ConsoleReporter, MultiReporter, JUnitReporter};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let config = load_config(std::path::Path::new("offload.toml"))?;
///
///     // Set up components
///     let provider = LocalProvider::new(Default::default());
///     let framework = PytestFramework::new(Default::default());
///     let reporter = MultiReporter::new()
///         .with_reporter(ConsoleReporter::new(true))
///         .with_reporter(JUnitReporter::new("results.xml".into()));
///
///     // Create orchestrator and run
///     let orchestrator = Orchestrator::new(config, "example".to_string(), provider, framework, reporter);
///     let sandbox_pool = Mutex::new(SandboxPool::new());
///     let result = orchestrator.run(&sandbox_pool).await?;
///
///     std::process::exit(result.exit_code());
/// }
/// ```
pub struct Orchestrator<P, D, R> {
    config: Config,
    group_name: String,
    provider: P,
    framework: D,
    reporter: R,
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
    /// * `framework` - Test framework for finding tests
    /// * `reporter` - Reporter for outputting results
    pub fn new(config: Config, group_name: String, provider: P, framework: D, reporter: R) -> Self {
        Self {
            config,
            group_name,
            provider,
            framework,
            reporter,
        }
    }

    /// Runs all tests and returns the aggregated results.
    ///
    /// This is the main entry point for test execution. It performs:
    ///
    /// 1. Test discovery using the configured framework
    /// 2. Scheduling tests into batches based on `max_parallel`
    /// 3. Parallel execution across sandboxes (reusing from pool or creating new)
    /// 4. Retrying failed tests (if `retry_count > 0`)
    /// 5. Aggregating results and notifying the reporter
    ///
    /// # Arguments
    ///
    /// * `sandbox_pool` - Pool of sandboxes to use. Takes from pool if available,
    ///   creates new sandboxes if needed. All sandboxes are returned to the pool
    ///   after execution.
    ///
    /// # Returns
    ///
    /// [`RunResult`] containing summary statistics and individual results.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Test discovery fails completely
    /// - All sandbox creations fail
    /// - Critical infrastructure errors occur
    pub async fn run(
        &self,
        sandbox_pool: &Mutex<SandboxPool<P::Sandbox>>,
    ) -> anyhow::Result<RunResult> {
        let start = std::time::Instant::now();

        // Clear output directory to avoid stale results
        let output_dir = &self.config.report.output_dir;
        if output_dir.exists() {
            std::fs::remove_dir_all(output_dir).ok();
        }
        std::fs::create_dir_all(output_dir).ok();

        // Discover tests
        info!("Discovering tests...");
        let paths: Vec<PathBuf> = Vec::new(); // Use default paths from config
        let tests = self.framework.discover(&paths).await?;

        if tests.is_empty() {
            warn!("No tests discovered");
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

        info!("Discovered {} tests", tests.len());
        self.reporter.on_discovery_complete(&tests).await;

        // Filter out skipped tests and create Test handles
        let tests_to_run: Vec<TestInstance<'_>> = tests
            .iter()
            .filter(|t| !t.skipped)
            .map(|t| t.test())
            .collect();

        let skipped_count = tests.len() - tests_to_run.len();

        // Schedule tests into batches
        let scheduler = Scheduler::new(self.config.offload.max_parallel);
        let batches = scheduler.schedule(&tests_to_run);

        info!(
            "Scheduled {} tests into {} batches",
            tests_to_run.len(),
            batches.len()
        );

        // Run tests in parallel
        let results = Mutex::new(Vec::new());
        let mut retry_manager = RetryManager::new(self.config.offload.retry_count);

        // Execute batches concurrently using scoped spawns (no 'static required)
        tokio_scoped::scope(|scope| {
            for (batch_idx, batch) in batches.into_iter().enumerate() {
                let results = &results;
                let provider = &self.provider;
                let framework = &self.framework;
                let reporter = &self.reporter;
                let config = &self.config;

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

                    // Enable streaming if configured
                    if config.offload.stream_output {
                        let callback: OutputCallback = Arc::new(|test_id, line| match line {
                            OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
                            OutputLine::Stderr(s) => eprintln!("[{}] {}", test_id, s),
                        });
                        runner = runner.with_streaming(callback);
                    }

                    for test in &batch {
                        reporter.on_test_start(test.record()).await;

                        match runner.run_test(test).await {
                            Ok(()) => {
                                // Result is now stored in the TestRecord
                                if let Some(r) = test.record().final_result() {
                                    reporter.on_test_complete(&r).await;
                                    results.lock().await.push(r);
                                }
                            }
                            Err(e) => {
                                error!("Test execution error: {}", e);
                                let failed_result = TestResult {
                                    test_id: test.id_owned(),
                                    outcome: TestOutcome::Error,
                                    duration: Duration::ZERO,
                                    stdout: String::new(),
                                    stderr: e.to_string(),
                                    error_message: Some(e.to_string()),
                                    stack_trace: None,
                                };
                                // Also record the error result into the TestRecord
                                test.record_result(failed_result.clone());
                                reporter.on_test_complete(&failed_result).await;
                                results.lock().await.push(failed_result);
                            }
                        }
                    }

                    // Return sandbox to pool for reuse (don't terminate)
                    let sandbox = runner.into_sandbox();
                    sandbox_pool.lock().await.add(sandbox);
                });
            }
        });

        // Collect results
        let mut all_results = results.into_inner();

        // Identify failed test IDs for retry
        let failed_test_ids: Vec<String> = all_results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error)
            .map(|r| r.test_id.clone())
            .collect();

        // Retry failed tests
        let mut flaky_count = 0;
        if !failed_test_ids.is_empty() && self.config.offload.retry_count > 0 {
            info!("Retrying {} failed tests...", failed_test_ids.len());

            // Get Test references for failed tests from the original records
            let failed_tests: Vec<TestInstance<'_>> = tests
                .iter()
                .filter(|r| failed_test_ids.contains(&r.id))
                .map(|r| r.test())
                .collect();

            let retry_results = self
                .retry_tests(&failed_tests, &mut retry_manager, sandbox_pool)
                .await?;

            // Update results based on retries
            for retry_result in retry_results {
                if retry_result.outcome == TestOutcome::Passed {
                    // Mark as flaky - passed on retry
                    flaky_count += 1;

                    // Update the original result
                    if let Some(original) = all_results
                        .iter_mut()
                        .find(|r| r.test_id == retry_result.test_id)
                    {
                        original.outcome = TestOutcome::Passed;
                        original.error_message = Some("Flaky - passed on retry".to_string());
                    }
                }
            }
        }

        // Calculate statistics
        let passed = all_results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Passed)
            .count();
        let failed = all_results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error)
            .count();
        let runtime_skipped = all_results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Skipped)
            .count();
        let not_run = tests_to_run.len().saturating_sub(all_results.len());

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

        self.reporter
            .on_run_complete(&run_result, &self.group_name)
            .await;

        Ok(run_result)
    }

    /// Retry failed tests using sandboxes from the pool.
    ///
    /// Tests are batched across available sandboxes and run in parallel,
    /// similar to the initial test run.
    async fn retry_tests(
        &self,
        tests: &[TestInstance<'_>],
        retry_manager: &mut RetryManager,
        sandbox_pool: &Mutex<SandboxPool<P::Sandbox>>,
    ) -> anyhow::Result<Vec<TestResult>> {
        // Filter to tests that should be retried
        let tests_to_retry: Vec<_> = tests
            .iter()
            .filter(|t| retry_manager.should_retry(t.id()))
            .copied()
            .collect();

        if tests_to_retry.is_empty() {
            return Ok(Vec::new());
        }

        // Check if we have sandboxes available
        if sandbox_pool.lock().await.is_empty() {
            warn!("No sandboxes available for retries");
            return Ok(Vec::new());
        }

        let retry_results = Mutex::new(Vec::new());
        let retry_manager = Mutex::new(retry_manager);

        // Run retries for each attempt
        for attempt in 0..self.config.offload.retry_count {
            // Get tests that still need retrying (haven't passed yet)
            let still_failing: Vec<_> = {
                let mgr = retry_manager.lock().await;
                tests_to_retry
                    .iter()
                    .filter(|t| mgr.should_retry(t.id()))
                    .copied()
                    .collect()
            };

            if still_failing.is_empty() {
                break;
            }

            info!(
                "Retry attempt {} for {} tests",
                attempt + 1,
                still_failing.len()
            );

            // Schedule tests across available sandboxes
            let num_sandboxes = sandbox_pool.lock().await.len();
            let scheduler = Scheduler::new(num_sandboxes);
            let batches = scheduler.schedule(&still_failing);

            // Execute retries in parallel
            tokio_scoped::scope(|scope| {
                for batch in batches.into_iter() {
                    let retry_results = &retry_results;
                    let retry_manager = &retry_manager;
                    let framework = &self.framework;
                    let config = &self.config;

                    scope.spawn(async move {
                        // Take sandbox from pool
                        let sandbox = match sandbox_pool.lock().await.take_one() {
                            Some(s) => s,
                            None => return,
                        };

                        let mut runner = TestRunner::new(
                            sandbox,
                            framework,
                            Duration::from_secs(config.offload.test_timeout_secs),
                        );

                        for test in batch {
                            match runner.run_test(&test).await {
                                Ok(()) => {
                                    if let Some(result) = test.record().final_result() {
                                        let passed = result.outcome == TestOutcome::Passed;
                                        retry_manager
                                            .lock()
                                            .await
                                            .record_attempt(test.id(), passed);

                                        if passed {
                                            retry_results.lock().await.push(result);
                                        }
                                    } else {
                                        retry_manager.lock().await.record_attempt(test.id(), false);
                                    }
                                }
                                Err(e) => {
                                    warn!("Retry failed for {}: {}", test.id(), e);
                                    retry_manager.lock().await.record_attempt(test.id(), false);
                                }
                            }
                        }

                        // Return sandbox to pool
                        let sandbox = runner.into_sandbox();
                        sandbox_pool.lock().await.add(sandbox);
                    });
                }
            });
        }

        Ok(retry_results.into_inner())
    }
}
