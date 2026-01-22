//! Test reporting and output generation.

pub mod junit;

use async_trait::async_trait;

use crate::discovery::{TestCase, TestResult};
use crate::executor::RunResult;

pub use junit::JUnitReporter;

/// A test reporter receives events during test execution.
#[async_trait]
pub trait Reporter: Send + Sync {
    /// Called when test discovery is complete.
    async fn on_discovery_complete(&self, tests: &[TestCase]);

    /// Called when a test starts running.
    async fn on_test_start(&self, test: &TestCase);

    /// Called when a test completes.
    async fn on_test_complete(&self, result: &TestResult);

    /// Called when all tests have completed.
    async fn on_run_complete(&self, result: &RunResult);
}

/// A reporter that does nothing (for testing or when output is not needed).
pub struct NullReporter;

#[async_trait]
impl Reporter for NullReporter {
    async fn on_discovery_complete(&self, _tests: &[TestCase]) {}
    async fn on_test_start(&self, _test: &TestCase) {}
    async fn on_test_complete(&self, _result: &TestResult) {}
    async fn on_run_complete(&self, _result: &RunResult) {}
}

/// A reporter that combines multiple reporters.
pub struct MultiReporter {
    reporters: Vec<Box<dyn Reporter>>,
}

impl MultiReporter {
    /// Create a new multi-reporter.
    pub fn new() -> Self {
        Self {
            reporters: Vec::new(),
        }
    }

    /// Add a reporter to the multi-reporter.
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
    async fn on_discovery_complete(&self, tests: &[TestCase]) {
        for reporter in &self.reporters {
            reporter.on_discovery_complete(tests).await;
        }
    }

    async fn on_test_start(&self, test: &TestCase) {
        for reporter in &self.reporters {
            reporter.on_test_start(test).await;
        }
    }

    async fn on_test_complete(&self, result: &TestResult) {
        for reporter in &self.reporters {
            reporter.on_test_complete(result).await;
        }
    }

    async fn on_run_complete(&self, result: &RunResult) {
        for reporter in &self.reporters {
            reporter.on_run_complete(result).await;
        }
    }
}

/// Console reporter that shows progress in the terminal.
pub struct ConsoleReporter {
    progress: std::sync::Mutex<Option<indicatif::ProgressBar>>,
    verbose: bool,
}

impl ConsoleReporter {
    /// Create a new console reporter.
    pub fn new(verbose: bool) -> Self {
        Self {
            progress: std::sync::Mutex::new(None),
            verbose,
        }
    }
}

#[async_trait]
impl Reporter for ConsoleReporter {
    async fn on_discovery_complete(&self, tests: &[TestCase]) {
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

    async fn on_test_start(&self, test: &TestCase) {
        if self.verbose {
            println!("Running: {}", test.id);
        }
    }

    async fn on_test_complete(&self, result: &TestResult) {
        if let Some(pb) = self.progress.lock().unwrap().as_ref() {
            pb.inc(1);

            let status = match result.outcome {
                crate::discovery::TestOutcome::Passed => console::style("PASS").green(),
                crate::discovery::TestOutcome::Failed => console::style("FAIL").red(),
                crate::discovery::TestOutcome::Skipped => console::style("SKIP").yellow(),
                crate::discovery::TestOutcome::Error => console::style("ERR ").red().bold(),
            };

            if self.verbose || result.outcome != crate::discovery::TestOutcome::Passed {
                pb.println(format!("{} {}", status, result.test.id));
            }
        }
    }

    async fn on_run_complete(&self, result: &RunResult) {
        if let Some(pb) = self.progress.lock().unwrap().take() {
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
                if r.outcome == crate::discovery::TestOutcome::Failed
                    || r.outcome == crate::discovery::TestOutcome::Error
                {
                    println!("  - {}", r.test.id);
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
