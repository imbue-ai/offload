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
//! 3. [`inc_progress`](Reporter::inc_progress) - When tests complete
//! 4. [`on_run_complete`](Reporter::on_run_complete) - After all tests finish
//!
//! # Built-in Reporters
//!
//! | Reporter | Description |
//! |----------|-------------|
//! | [`ConsoleReporter`] | Terminal output with progress bar |
//! | [`MultiReporter`] | Combines multiple reporters |
//! | [`NullReporter`] | Discards all events (for testing) |
//!
//! # Combining Reporters
//!
//! Use [`MultiReporter`] to send events to multiple reporters:
//!
//! ```
//! use offload::report::{MultiReporter, ConsoleReporter};
//!
//! let reporter = MultiReporter::new()
//!     .with_reporter(ConsoleReporter::new(true));
//! ```

pub mod junit;

pub use junit::{
    MasterJunitReport, SharedJunitReport, TestStatus, cleanup_parts, merge_junit_files,
};

use async_trait::async_trait;

use crate::framework::TestRecord;
use crate::orchestrator::RunResult;

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
/// 2. `on_test_start` / `inc_progress` (per batch, possibly concurrent)
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

    /// Increment the progress bar.
    async fn inc_progress(&self, count: usize);

    /// Called when all tests have completed.
    ///
    /// Receives the aggregated run result with all test outcomes.
    /// Use this for final summary output or file generation.
    async fn on_run_complete(&self, result: &RunResult);
}

/// A reporter that discards all events.
///
/// Useful for testing or when no output is desired.
///
/// # Example
///
/// ```
/// use offload::report::NullReporter;
///
/// let reporter = NullReporter;
/// ```
pub struct NullReporter;

#[async_trait]
impl Reporter for NullReporter {
    async fn on_discovery_complete(&self, _tests: &[TestRecord]) {}
    async fn on_test_start(&self, _test: &TestRecord) {}
    async fn inc_progress(&self, _count: usize) {}
    async fn on_run_complete(&self, _result: &RunResult) {}
}

/// A reporter that forwards events to multiple child reporters.
///
/// Use this to combine different output formats.
/// Events are sent to all child reporters in the order they were added.
///
/// # Example
///
/// ```
/// use offload::report::{MultiReporter, ConsoleReporter, NullReporter};
///
/// let reporter = MultiReporter::new()
///     .with_reporter(ConsoleReporter::new(true));
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
    /// use offload::report::{MultiReporter, ConsoleReporter};
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

    async fn inc_progress(&self, count: usize) {
        for reporter in &self.reporters {
            reporter.inc_progress(count).await;
        }
    }

    async fn on_run_complete(&self, result: &RunResult) {
        for reporter in &self.reporters {
            reporter.on_run_complete(result).await;
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
/// use offload::report::ConsoleReporter;
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
        // Count total instances (including retries)
        let total_instances: usize = tests
            .iter()
            .filter(|t| !t.skipped)
            .map(|t| t.retry_count + 1)
            .sum();

        let pb = indicatif::ProgressBar::new(total_instances as u64);
        if let Ok(style) = indicatif::ProgressStyle::default_bar().template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
        ) {
            pb.set_style(style.progress_chars("#>-"));
        }
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        if let Ok(mut guard) = self.progress.lock() {
            *guard = Some(pb);
        }
    }

    async fn on_test_start(&self, test: &TestRecord) {
        if self.verbose {
            println!("Running: {}", test.id);
        }
    }

    async fn inc_progress(&self, count: usize) {
        if let Ok(guard) = self.progress.lock()
            && let Some(pb) = guard.as_ref()
        {
            pb.inc(count as u64);
        }
    }

    async fn on_run_complete(&self, result: &RunResult) {
        if let Ok(mut guard) = self.progress.lock()
            && let Some(pb) = guard.take()
        {
            pb.finish_and_clear();
        }

        println!();
        println!("Test Results:");
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
            println!("{}", console::style("All tests passed!").green().bold());
        } else if result.not_run > 0 && result.failed == 0 {
            println!();
            println!(
                "{}",
                console::style("No test results were collected.")
                    .red()
                    .bold()
            );
            println!(
                "{}",
                console::style("Ensure tests generate JUnit XML at /tmp/junit.xml and download_command is configured.")
                    .dim()
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
