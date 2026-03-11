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

    if show_cost && result.estimated_cost.estimated_cost_usd > 0.0 {
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
