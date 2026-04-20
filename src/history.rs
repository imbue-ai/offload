//! Historical test statistics storage and queries.
//!
//! This module provides types for storing and querying historical test behavior,
//! including failure rates, duration percentiles, and recent run results.

pub mod jsonl;
pub mod reservoir;

use std::time::Duration;

/// Statistics about a single test's historical behavior.
#[derive(Debug, Clone)]
pub struct TestStatistics {
    /// Canonical test identifier.
    pub test_id: String,
    /// Configuration file these statistics are from.
    pub config: String,
    /// Total number of attempts recorded.
    pub total_attempts: u64,
    /// Total number of failures recorded.
    pub total_failures: u64,
    /// Failure rate: total_failures / total_attempts.
    pub failure_rate: f64,
    /// Duration statistics split by outcome (in seconds).
    pub duration: OutcomeStats,
    /// Timestamp of most recent attempt (Unix epoch milliseconds).
    pub last_attempt_ms: u64,
    /// Run ID of the most recent run that included this test.
    pub last_run_id: String,
}

/// Duration percentile statistics for a set of test samples.
#[derive(Debug, Clone)]
pub struct DurationStats {
    /// Estimated median (P50) duration.
    pub p50_secs: f64,
    /// Estimated 75th percentile duration.
    pub p75_secs: f64,
    /// Estimated 90th percentile duration.
    pub p90_secs: f64,
    /// Estimated 95th percentile duration.
    pub p95_secs: f64,
}

/// Statistics split by outcome. Each test stores separate reservoirs
/// for successes and failures, so percentiles are computed independently.
#[derive(Debug, Clone)]
pub struct OutcomeStats {
    /// Duration statistics from the success reservoir.
    pub success: Option<DurationStats>,
    /// Duration statistics from the failure reservoir.
    pub failure: Option<DurationStats>,
}

/// Result of a single test attempt, used for recording.
#[derive(Debug, Clone)]
pub struct TestAttemptResult {
    /// Configuration file name.
    pub config: String,
    /// Canonical test identifier.
    pub test_id: String,
    /// Run ID for this attempt.
    pub run_id: String,
    /// Whether the test passed.
    pub passed: bool,
    /// Test duration in seconds.
    pub duration_secs: f64,
    /// Timestamp in Unix epoch milliseconds.
    pub timestamp_ms: u64,
}

/// Errors that can occur when working with test history.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Failed to parse history data.
    #[error("Parse error: {0}")]
    Parse(String),
    /// History storage is disabled in configuration.
    #[error("History storage is disabled")]
    Disabled,
}

/// Trait for querying historical test statistics.
///
/// Implementations may be backed by local files, databases, or return
/// default estimates when no history is available.
///
/// This trait does NOT require Send + Sync. The history store is used
/// single-threaded: loaded after the parallel test run completes,
/// mutated to record results, then saved.
pub trait TestHistoryStore {
    /// Get statistics for a specific test.
    ///
    /// Returns None if no history exists for this test.
    fn get_stats(&self, config: &str, test_id: &str) -> Option<TestStatistics>;

    /// Get statistics for all tests matching a config.
    fn get_all_stats(&self, config: &str) -> Vec<TestStatistics>;

    /// Get the N tests with highest failure rate.
    ///
    /// Uses the all-time counters (total_failures / total_attempts) for
    /// statistical stability.
    fn flakiest_tests(&self, config: &str, limit: usize) -> Vec<TestStatistics>;

    /// Get the N slowest tests.
    ///
    /// The ranking metric is an implementation detail; callers should not
    /// depend on which specific percentile or reservoir is used.
    fn slowest_tests(&self, config: &str, limit: usize) -> Vec<TestStatistics>;

    /// Get tests that failed in the most recent run.
    ///
    /// Derives the most recent run ID by finding max(last_run) across all
    /// tests for this config.
    fn last_run_failures(&self, config: &str) -> Vec<String>;

    /// Get expected duration for scheduling purposes.
    ///
    /// Falls back through: test weighted P75 -> group average -> configurable default.
    fn expected_duration(&self, config: &str, test_id: &str) -> Duration;

    /// Record results from a completed test run.
    ///
    /// Called after each offload run completes.
    fn record_results(&mut self, results: &[TestAttemptResult]) -> Result<(), HistoryError>;
}
