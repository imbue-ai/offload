//! Test reporting and output generation.
//!
//! This module provides the [`Reporter`] trait for receiving test events
//! and several built-in reporter implementations.
//!
//! # Reporter Trait
//!
//! Reporters receive callbacks during test execution:
//!
//! 1. [`on_discovery_complete`](Reporter::on_discovery_complete) - After tests are found
//! 2. [`on_test_start`](Reporter::on_test_start) - Before each test runs
//! 3. [`on_test_complete`](Reporter::on_test_complete) - After each test finishes
//! 4. [`on_run_complete`](Reporter::on_run_complete) - After all tests finish
//!
//! # Built-in Reporters
//!
//! | Reporter | Description |
//! |----------|-------------|
//! | [`ConsoleReporter`] | Terminal output with progress bar |
//! | [`JUnitReporter`] | JUnit XML file for CI systems |
//! | [`MultiReporter`] | Combines multiple reporters |
//! | [`NullReporter`] | Discards all events (for testing) |
//!
//! # Combining Reporters
//!
//! Use [`MultiReporter`] to send events to multiple reporters:
//!
//! ```
//! use shotgun::report::{MultiReporter, ConsoleReporter, JUnitReporter};
//!
//! let reporter = MultiReporter::new()
//!     .with_reporter(ConsoleReporter::new(true))
//!     .with_reporter(JUnitReporter::new("results.xml".into()));
//! ```

pub mod junit;

use async_trait::async_trait;

use crate::framework::{TestRecord, TestResult};
use crate::orchestrator::RunResult;

pub use junit::JUnitReporter;

/// Trait for receiving test execution events.
///
/// Reporters are notified at key points during test execution and can
/// output results in various formats (terminal, files, webhooks, etc.).
///
/// # Thread Safety
///
/// Reporters must be `Send + Sync` as events may arrive from multiple
/// async tasks concurrently.
///
/// # Event Order
///
/// Events are delivered in this order:
/// 1. `on_discovery_complete` (once)
/// 2. `on_test_start` / `on_test_complete` (per test, possibly concurrent)
/// 3. `on_run_complete` (once)
#[async_trait]
pub trait Reporter: Send + Sync {
    /// Called when test discovery is complete.
    ///
    /// Receives the full list of discovered tests before execution begins.
    /// Useful for setting up progress tracking or initial reporting.
    async fn on_discovery_complete(&self, tests: &[TestRecord]);

    /// Called when a test starts executing.
    ///
    /// May be called concurrently for parallel tests.
    async fn on_test_start(&self, test: &TestRecord);

    /// Called when a test completes with its result.
    ///
    /// May be called concurrently for parallel tests.
    async fn on_test_complete(&self, result: &TestResult);

    /// Called when all tests have completed.
    ///
    /// Receives the aggregated run result with all test outcomes.
    /// Use this for final summary output or file generation.
    async fn on_run_complete(&self, result: &RunResult, group_name: &str);
}

/// A reporter that discards all events.
///
/// Useful for testing or when no output is desired.
///
/// # Example
///
/// ```
/// use shotgun::report::NullReporter;
///
/// let reporter = NullReporter;
/// ```
pub struct NullReporter;

#[async_trait]
impl Reporter for NullReporter {
    async fn on_discovery_complete(&self, _tests: &[TestRecord]) {}
    async fn on_test_start(&self, _test: &TestRecord) {}
    async fn on_test_complete(&self, _result: &TestResult) {}
    async fn on_run_complete(&self, _result: &RunResult, _group_name: &str) {}
}

/// A reporter that forwards events to multiple child reporters.
///
/// Use this to combine different output formats (e.g., console + JUnit).
/// Events are sent to all child reporters in the order they were added.
///
/// # Example
///
/// ```
/// use shotgun::report::{MultiReporter, ConsoleReporter, JUnitReporter, NullReporter};
///
/// let reporter = MultiReporter::new()
///     .with_reporter(ConsoleReporter::new(true))
///     .with_reporter(JUnitReporter::new("test-results/junit.xml".into()));
/// ```
pub struct MultiReporter {
    reporters: Vec<Box<dyn Reporter>>,
}

impl MultiReporter {
    /// Creates a new empty multi-reporter.
    ///
    /// Use [`with_reporter`](Self::with_reporter) to add child reporters.
    pub fn new() -> Self {
        Self {
            reporters: Vec::new(),
        }
    }

    /// Adds a reporter to receive events.
    ///
    /// Returns `self` for method chaining.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::report::{MultiReporter, ConsoleReporter};
    ///
    /// let reporter = MultiReporter::new()
    ///     .with_reporter(ConsoleReporter::new(false));
    /// ```
    pub fn with_reporter<R: Reporter + 'static>(mut self, reporter: R) -> Self {
        self.reporters.push(Box::new(reporter));
        self
    }
}

impl Default for MultiReporter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Reporter for MultiReporter {
    async fn on_discovery_complete(&self, tests: &[TestRecord]) {
        for reporter in &self.reporters {
            reporter.on_discovery_complete(tests).await;
        }
    }

    async fn on_test_start(&self, test: &TestRecord) {
        for reporter in &self.reporters {
            reporter.on_test_start(test).await;
        }
    }

    async fn on_test_complete(&self, result: &TestResult) {
        for reporter in &self.reporters {
            reporter.on_test_complete(result).await;
        }
    }

    async fn on_run_complete(&self, result: &RunResult, group_name: &str) {
        for reporter in &self.reporters {
            reporter.on_run_complete(result, group_name).await;
        }
    }
}

/// Terminal reporter with progress bar and colored output.
///
/// Provides real-time test progress using a progress bar and outputs
/// colored pass/fail status for each test. At the end, prints a summary
/// with failed test details.
///
/// # Output Modes
///
/// - **Normal** (`verbose: false`): Shows only failures
/// - **Verbose** (`verbose: true`): Shows all test results
///
/// # Example
///
/// ```
/// use shotgun::report::ConsoleReporter;
///
/// // Show all results
/// let verbose_reporter = ConsoleReporter::new(true);
///
/// // Show only failures
/// let quiet_reporter = ConsoleReporter::new(false);
/// ```
pub struct ConsoleReporter {
    progress: std::sync::Mutex<Option<indicatif::ProgressBar>>,
    verbose: bool,
}

impl ConsoleReporter {
    /// Creates a new console reporter.
    ///
    /// # Arguments
    ///
    /// * `verbose` - If `true`, prints all test results. If `false`,
    ///   only prints failures and the final summary.
    pub fn new(verbose: bool) -> Self {
        Self {
            progress: std::sync::Mutex::new(None),
            verbose,
        }
    }
}

#[async_trait]
impl Reporter for ConsoleReporter {
    async fn on_discovery_complete(&self, tests: &[TestRecord]) {
        println!("Discovered {} tests", tests.len());

        let pb = indicatif::ProgressBar::new(tests.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );

        *self.progress.lock().unwrap() = Some(pb);
    }

    async fn on_test_start(&self, test: &TestRecord) {
        if self.verbose {
            println!("Running: {}", test.id);
        }
    }

    async fn on_test_complete(&self, result: &TestResult) {
        if let Some(pb) = self.progress.lock().unwrap().as_ref() {
            pb.inc(1);

            let status = match result.outcome {
                crate::framework::TestOutcome::Passed => console::style("PASS").green(),
                crate::framework::TestOutcome::Failed => console::style("FAIL").red(),
                crate::framework::TestOutcome::Skipped => console::style("SKIP").yellow(),
                crate::framework::TestOutcome::Error => console::style("ERR ").red().bold(),
            };

            if self.verbose || result.outcome != crate::framework::TestOutcome::Passed {
                pb.println(format!("{} {}", status, result.test_id));
            }
        }
    }

    async fn on_run_complete(&self, result: &RunResult, group_name: &str) {
        if let Some(pb) = self.progress.lock().unwrap().take() {
            pb.finish_and_clear();
        }

        println!();
        println!("Test Results for group {}:", group_name);
        println!("  Total:   {}", result.total_tests);
        println!("  Passed:  {}", console::style(result.passed).green());
        println!("  Failed:  {}", console::style(result.failed).red());
        println!("  Skipped: {}", console::style(result.skipped).yellow());

        if result.not_run > 0 {
            println!("  Not Run: {}", console::style(result.not_run).red().bold());
        }

        if result.flaky > 0 {
            println!("  Flaky:   {}", console::style(result.flaky).yellow());
        }

        println!("  Duration: {:?}", result.duration);

        if result.success() {
            println!();
            println!(
                "{} '{}' {}",
                console::style("All tests in group").green().bold(),
                console::style(group_name).bold(),
                console::style("passed!").green().bold()
            );
        } else if result.not_run > 0 && result.failed == 0 {
            println!();
            println!(
                "{}",
                console::style("Tests could not be executed (sandbox creation failed).")
                    .red()
                    .bold()
            );
        } else {
            println!();
            println!("{}", console::style("Some tests failed.").red().bold());

            // Print failed test names
            println!();
            println!("Failed tests:");
            for r in &result.results {
                if r.outcome == crate::framework::TestOutcome::Failed
                    || r.outcome == crate::framework::TestOutcome::Error
                {
                    println!("  - {}", r.test_id);
                    if let Some(msg) = &r.error_message {
                        println!("    {}", console::style(msg).dim());
                    }
                    // Show stdout/stderr for failed tests when not streaming
                    if !r.stdout.is_empty() {
                        println!();
                        println!("    {}", console::style("stdout:").dim());
                        for line in r.stdout.lines() {
                            println!("      {}", line);
                        }
                    }
                    if !r.stderr.is_empty() {
                        println!();
                        println!("    {}", console::style("stderr:").dim());
                        for line in r.stderr.lines() {
                            println!("      {}", line);
                        }
                    }
                }
            }
        }
    }
}
