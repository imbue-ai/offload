//! Test execution engine.
//!
//! This module coordinates test discovery, distribution, execution,
//! and result collection across multiple sandboxes.

pub mod retry;
pub mod runner;
pub mod scheduler;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::{Config, SandboxConfig, SandboxResources};
use crate::discovery::{TestCase, TestDiscoverer, TestOutcome, TestResult};
use crate::provider::{OutputLine, Sandbox, SandboxProvider};
use crate::report::Reporter;

pub use retry::RetryManager;
pub use runner::{OutputCallback, TestRunner};
pub use scheduler::Scheduler;

/// Result of the entire test run.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Total number of tests discovered.
    pub total_tests: usize,
    /// Number of tests that passed.
    pub passed: usize,
    /// Number of tests that failed.
    pub failed: usize,
    /// Number of tests that were skipped.
    pub skipped: usize,
    /// Number of tests that were flaky (passed after retry).
    pub flaky: usize,
    /// Number of tests that were not run (e.g., sandbox creation failed).
    pub not_run: usize,
    /// Total duration of the test run.
    pub duration: Duration,
    /// Individual test results.
    pub results: Vec<TestResult>,
}

impl RunResult {
    /// Check if the overall run was successful.
    pub fn success(&self) -> bool {
        self.failed == 0 && self.not_run == 0
    }

    /// Get the exit code for this run result.
    pub fn exit_code(&self) -> i32 {
        if self.failed > 0 || self.not_run > 0 {
            1
        } else if self.flaky > 0 {
            34 // Same as original test_shotgun
        } else {
            0
        }
    }
}

/// The main orchestrator that coordinates test execution.
pub struct Orchestrator<P, D, R> {
    config: Config,
    provider: Arc<P>,
    discoverer: Arc<D>,
    reporter: Arc<R>,
}

impl<P, D, R> Orchestrator<P, D, R>
where
    P: SandboxProvider + 'static,
    D: TestDiscoverer + 'static,
    R: Reporter + 'static,
{
    /// Create a new orchestrator with the given components.
    pub fn new(config: Config, provider: Arc<P>, discoverer: Arc<D>, reporter: Arc<R>) -> Self {
        Self {
            config,
            provider,
            discoverer,
            reporter,
        }
    }

    /// Run all tests and return the results.
    pub async fn run(&self) -> anyhow::Result<RunResult> {
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
        let tests = self.discoverer.discover(&paths).await?;

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

        // Filter out skipped tests
        let tests_to_run: Vec<_> = tests.iter().filter(|t| !t.skipped).cloned().collect();

        let skipped_count = tests.len() - tests_to_run.len();

        // Schedule tests into batches
        let scheduler = Scheduler::new(self.config.shotgun.max_parallel);
        let batches = scheduler.schedule(&tests_to_run);

        info!(
            "Scheduled {} tests into {} batches",
            tests_to_run.len(),
            batches.len()
        );

        // Run tests in parallel
        let results = Arc::new(Mutex::new(Vec::new()));
        let mut retry_manager = RetryManager::new(self.config.shotgun.retry_count);

        // Execute batches concurrently
        let mut handles = Vec::new();

        for (batch_idx, batch) in batches.into_iter().enumerate() {
            let provider = self.provider.clone();
            let discoverer = self.discoverer.clone();
            let reporter = self.reporter.clone();
            let results = results.clone();
            let config = self.config.clone();

            let handle = tokio::spawn(async move {
                // Create initial sandbox to check if it's single-use
                let sandbox_config = SandboxConfig {
                    id: format!("shotgun-{}-{}", uuid::Uuid::new_v4(), batch_idx),
                    working_dir: config
                        .shotgun
                        .working_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string()),
                    env: Vec::new(),
                    resources: SandboxResources {
                        cpu: None,
                        memory: None,
                        timeout_secs: Some(config.shotgun.test_timeout_secs),
                    },
                };

                let initial_sandbox = match provider.create_sandbox(&sandbox_config).await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to create sandbox: {}", e);
                        return;
                    }
                };

                let mut runner = TestRunner::new(
                    initial_sandbox,
                    discoverer.clone(),
                    Duration::from_secs(config.shotgun.test_timeout_secs),
                );

                // Enable streaming if configured
                if config.shotgun.stream_output {
                    let callback: OutputCallback = Arc::new(|test_id, line| match line {
                        OutputLine::Stdout(s) => println!("[{}] {}", test_id, s),
                        OutputLine::Stderr(s) => eprintln!("[{}] {}", test_id, s),
                    });
                    runner = runner.with_streaming(callback);
                }

                for test in &batch {
                    reporter.on_test_start(test).await;

                    let result = runner.run_test(test).await;

                    match &result {
                        Ok(r) => {
                            reporter.on_test_complete(r).await;
                            results.lock().await.push(r.clone());
                        }
                        Err(e) => {
                            error!("Test execution error: {}", e);
                            let failed_result = TestResult {
                                test: test.clone(),
                                outcome: TestOutcome::Error,
                                duration: Duration::ZERO,
                                stdout: String::new(),
                                stderr: e.to_string(),
                                error_message: Some(e.to_string()),
                                stack_trace: None,
                            };
                            reporter.on_test_complete(&failed_result).await;
                            results.lock().await.push(failed_result);
                        }
                    }
                }

                // Terminate sandbox after all tests
                if let Err(e) = runner.sandbox().terminate().await {
                    warn!("Failed to terminate sandbox: {}", e);
                }
            });

            handles.push(handle);
        }

        // Wait for all batches to complete
        for handle in handles {
            if let Err(e) = handle.await {
                error!("Batch execution error: {}", e);
            }
        }

        // Collect results
        let mut all_results = results.lock().await.clone();

        // Identify failed tests for retry
        let failed_tests: Vec<_> = all_results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error)
            .map(|r| r.test.clone())
            .collect();

        // Retry failed tests
        let mut flaky_count = 0;
        if !failed_tests.is_empty() && self.config.shotgun.retry_count > 0 {
            info!("Retrying {} failed tests...", failed_tests.len());

            let retry_results = self.retry_tests(&failed_tests, &mut retry_manager).await?;

            // Update results based on retries
            for retry_result in retry_results {
                if retry_result.outcome == TestOutcome::Passed {
                    // Mark as flaky - passed on retry
                    flaky_count += 1;

                    // Update the original result
                    if let Some(original) = all_results
                        .iter_mut()
                        .find(|r| r.test.id == retry_result.test.id)
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
        let not_run = tests_to_run.len().saturating_sub(all_results.len());

        let run_result = RunResult {
            total_tests: tests.len(),
            passed,
            failed,
            skipped: skipped_count,
            flaky: flaky_count,
            not_run,
            duration: start.elapsed(),
            results: all_results,
        };

        self.reporter.on_run_complete(&run_result).await;

        Ok(run_result)
    }

    /// Retry failed tests.
    async fn retry_tests(
        &self,
        tests: &[TestCase],
        retry_manager: &mut RetryManager,
    ) -> anyhow::Result<Vec<TestResult>> {
        let mut retry_results = Vec::new();

        for test in tests {
            if !retry_manager.should_retry(&test.id) {
                continue;
            }

            for attempt in 0..retry_manager.max_retries() {
                let sandbox_config = SandboxConfig {
                    id: format!("shotgun-retry-{}-{}", uuid::Uuid::new_v4(), attempt),
                    working_dir: self
                        .config
                        .shotgun
                        .working_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string()),
                    env: Vec::new(),
                    resources: SandboxResources::default(),
                };

                let sandbox = match self.provider.create_sandbox(&sandbox_config).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("Failed to create retry sandbox: {}", e);
                        continue;
                    }
                };

                let mut runner = TestRunner::new(
                    sandbox,
                    self.discoverer.clone(),
                    Duration::from_secs(self.config.shotgun.test_timeout_secs),
                );

                match runner.run_test(test).await {
                    Ok(result) => {
                        retry_manager
                            .record_attempt(&test.id, result.outcome == TestOutcome::Passed);

                        if result.outcome == TestOutcome::Passed {
                            retry_results.push(result);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Retry attempt {} failed: {}", attempt + 1, e);
                        retry_manager.record_attempt(&test.id, false);
                    }
                }

                if let Err(e) = runner.sandbox().terminate().await {
                    warn!("Failed to terminate retry sandbox: {}", e);
                }
            }
        }

        Ok(retry_results)
    }
}
