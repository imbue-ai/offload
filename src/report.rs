//! Test reporting and output generation.
//!
//! This module provides utilities for test result reporting including
//! JUnit XML generation and console output formatting.

pub mod junit;

pub use junit::{
    FailureXml, MasterJunitReport, SharedJunitReport, TestStatus, TestcaseXml, TestsuiteXml,
    load_test_durations, parse_all_testsuites_xml,
};

use crate::orchestrator::RunResult;

/// Returns true if cost should be displayed in the summary.
pub(crate) fn should_show_cost(show_cost: bool, estimated_cost_usd: f64) -> bool {
    show_cost && estimated_cost_usd > 0.0
}

/// Prints a summary of test results to the console.
///
/// Displays pass/fail counts with colored output and appropriate
/// status messages based on the results. When `show_cost` is true
/// and the estimated cost is non-zero, displays the cost as well.
pub fn print_summary(result: &RunResult, show_cost: bool) {
    println!();
    println!("Test Results:");
    println!("  Total:   {}", result.total_tests);
    println!("  Passed:  {}", console::style(result.passed).green());
    println!("  Failed:  {}", console::style(result.failed).red());

    if result.not_run > 0 {
        println!("  Not Run: {}", console::style(result.not_run).red().bold());
    }

    if result.flaky > 0 {
        println!("  Flaky:   {}", console::style(result.flaky).yellow());
    }

    println!("  Duration: {:?}", result.duration);

    if should_show_cost(show_cost, result.estimated_cost.estimated_cost_usd) {
        println!("  {}", result.estimated_cost);
    }

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
            console::style(
                "Ensure tests generate JUnit XML at /tmp/junit.xml and download_command is configured."
            )
            .dim()
        );
    } else {
        println!();
        println!("{}", console::style("Some tests failed.").red().bold());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CostEstimate;
    use std::time::Duration;

    fn make_result(cost_usd: f64) -> RunResult {
        RunResult {
            total_tests: 10,
            passed: 10,
            failed: 0,
            flaky: 0,
            not_run: 0,
            duration: Duration::from_secs(5),
            estimated_cost: CostEstimate {
                cpu_seconds: 100.0,
                gpu_seconds: 0.0,
                estimated_cost_usd: cost_usd,
            },
        }
    }

    #[test]
    fn should_show_cost_when_flag_set_and_cost_positive() {
        assert!(should_show_cost(true, 0.01));
    }

    #[test]
    fn should_not_show_cost_when_flag_unset() {
        assert!(!should_show_cost(false, 0.01));
    }

    #[test]
    fn should_not_show_cost_when_cost_is_zero() {
        assert!(!should_show_cost(true, 0.0));
    }

    #[test]
    fn should_not_show_cost_when_flag_unset_and_cost_zero() {
        assert!(!should_show_cost(false, 0.0));
    }

    #[test]
    fn print_summary_does_not_panic_with_cost() {
        let result = make_result(0.05);
        print_summary(&result, true);
    }

    #[test]
    fn print_summary_does_not_panic_without_cost() {
        let result = make_result(0.0);
        print_summary(&result, false);
    }
}
