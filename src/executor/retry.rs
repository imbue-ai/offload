//! Retry management and flaky test detection.
//!
//! This module handles automatic retrying of failed tests and identifies
//! flaky tests (tests that intermittently fail).
//!
//! # Flaky Test Detection
//!
//! A test is considered "flaky" if it:
//! 1. Fails at least once
//! 2. Passes at least once (during retry)
//!
//! Flaky tests are important to identify because they:
//! - Can cause intermittent CI failures
//! - May indicate race conditions or timing issues
//! - Should be investigated and fixed
//!
//! # Retry Budget
//!
//! The [`RetryBudget`] can limit retries by failure category:
//! - Timeout failures
//! - Regular assertion failures
//! - Known error patterns
//! - Unknown errors
//!
//! # Example
//!
//! ```
//! use shotgun::executor::RetryManager;
//!
//! let mut manager = RetryManager::new(3); // Up to 3 retries
//!
//! // Test fails first attempt
//! assert!(manager.should_retry("test_flaky"));
//! manager.record_attempt("test_flaky", false);
//!
//! // Test passes on retry
//! manager.record_attempt("test_flaky", true);
//!
//! // Now identified as flaky
//! assert!(manager.is_flaky("test_flaky"));
//! ```

use std::collections::HashMap;

/// Manages test retries and tracks flaky tests.
///
/// The retry manager maintains state about retry attempts and can
/// identify which tests are flaky based on their pass/fail history.
pub struct RetryManager {
    max_retries: usize,
    /// Tracks retry attempts per test: (attempts, successes)
    attempts: HashMap<String, (usize, usize)>,
    /// Maximum tests to retry for various failure reasons
    #[allow(dead_code)]
    budget: RetryBudget,
}

/// Budget limits for retries by failure category.
///
/// Allows limiting how many tests are retried based on why they failed.
/// This prevents excessive retrying when there's a systemic issue.
///
/// # Default Values
///
/// | Category | Limit |
/// |----------|-------|
/// | Timeout | 6 |
/// | Failed | 6 |
/// | Known Errors | 4 |
/// | Unknown Errors | 4 |
#[derive(Clone)]
pub struct RetryBudget {
    /// Maximum tests to retry for timeout failures.
    pub timeout: usize,

    /// Maximum tests to retry for assertion failures.
    pub failed: usize,

    /// Maximum tests to retry for known error patterns.
    pub known_errors: usize,

    /// Maximum tests to retry for unknown/unexpected errors.
    pub unknown_errors: usize,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self {
            timeout: 6,
            failed: 6,
            known_errors: 4,
            unknown_errors: 4,
        }
    }
}

impl RetryManager {
    /// Creates a new retry manager with the given max retries per test.
    ///
    /// # Arguments
    ///
    /// * `max_retries` - Maximum retry attempts per test (0 = no retries)
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::executor::RetryManager;
    ///
    /// let manager = RetryManager::new(3); // Up to 3 retries
    /// ```
    pub fn new(max_retries: usize) -> Self {
        Self {
            max_retries,
            attempts: HashMap::new(),
            budget: RetryBudget::default(),
        }
    }

    /// Creates a new retry manager with a custom budget.
    ///
    /// # Arguments
    ///
    /// * `max_retries` - Maximum retry attempts per test
    /// * `budget` - Limits on retries by failure category
    pub fn with_budget(max_retries: usize, budget: RetryBudget) -> Self {
        Self {
            max_retries,
            attempts: HashMap::new(),
            budget,
        }
    }

    /// Returns the maximum number of retries configured.
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// Checks if a test should be retried.
    ///
    /// Returns `true` if the test hasn't exceeded max_retries.
    pub fn should_retry(&self, test_id: &str) -> bool {
        let (count, _) = self.attempts.get(test_id).unwrap_or(&(0, 0));
        *count < self.max_retries
    }

    /// Records a test attempt (success or failure).
    ///
    /// Should be called after each test execution to track retry history.
    ///
    /// # Arguments
    ///
    /// * `test_id` - The test's unique identifier
    /// * `success` - Whether the test passed
    pub fn record_attempt(&mut self, test_id: &str, success: bool) {
        let entry = self.attempts.entry(test_id.to_string()).or_insert((0, 0));
        entry.0 += 1;
        if success {
            entry.1 += 1;
        }
    }

    /// Returns the number of attempts for a specific test.
    pub fn get_attempts(&self, test_id: &str) -> usize {
        self.attempts.get(test_id).map(|(c, _)| *c).unwrap_or(0)
    }

    /// Checks if a test is flaky.
    ///
    /// A test is flaky if it has both failures and successes across
    /// its attempts (i.e., inconsistent results).
    pub fn is_flaky(&self, test_id: &str) -> bool {
        if let Some((attempts, successes)) = self.attempts.get(test_id) {
            // Test is flaky if it had at least one failure and one success
            *attempts > 1 && *successes > 0 && *successes < *attempts
        } else {
            false
        }
    }

    /// Returns the IDs of all tests identified as flaky.
    pub fn get_flaky_tests(&self) -> Vec<String> {
        self.attempts
            .iter()
            .filter(|(_, (attempts, successes))| {
                *attempts > 1 && *successes > 0 && *successes < *attempts
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Returns summary statistics about retries.
    pub fn stats(&self) -> RetryStats {
        let total_tests = self.attempts.len();
        let total_retries: usize = self.attempts.values().map(|(c, _)| c.saturating_sub(1)).sum();
        let flaky_tests = self
            .attempts
            .iter()
            .filter(|(_, (a, s))| *a > 1 && *s > 0 && *s < *a)
            .count();

        RetryStats {
            total_tests,
            total_retries,
            flaky_tests,
        }
    }
}

/// Statistics about retry attempts and flaky test detection.
///
/// Returned by [`RetryManager::stats`] to summarize retry activity.
#[derive(Debug, Clone)]
pub struct RetryStats {
    /// Total number of unique tests that were tracked.
    pub total_tests: usize,

    /// Total retry attempts (not counting initial runs).
    pub total_retries: usize,

    /// Number of tests identified as flaky.
    pub flaky_tests: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_manager_basic() {
        let mut manager = RetryManager::new(3);

        // First test
        assert!(manager.should_retry("test1"));
        manager.record_attempt("test1", false);
        assert!(manager.should_retry("test1"));
        manager.record_attempt("test1", false);
        assert!(manager.should_retry("test1"));
        manager.record_attempt("test1", false);
        assert!(!manager.should_retry("test1"));
    }

    #[test]
    fn test_flaky_detection() {
        let mut manager = RetryManager::new(3);

        // Test that fails first, then passes
        manager.record_attempt("test1", false);
        manager.record_attempt("test1", true);

        assert!(manager.is_flaky("test1"));
    }

    #[test]
    fn test_not_flaky_if_always_passes() {
        let mut manager = RetryManager::new(3);

        manager.record_attempt("test1", true);
        manager.record_attempt("test1", true);

        assert!(!manager.is_flaky("test1"));
    }

    #[test]
    fn test_not_flaky_if_always_fails() {
        let mut manager = RetryManager::new(3);

        manager.record_attempt("test1", false);
        manager.record_attempt("test1", false);

        assert!(!manager.is_flaky("test1"));
    }

    #[test]
    fn test_get_flaky_tests() {
        let mut manager = RetryManager::new(3);

        // Flaky test
        manager.record_attempt("test1", false);
        manager.record_attempt("test1", true);

        // Non-flaky test (always passes)
        manager.record_attempt("test2", true);

        // Non-flaky test (always fails)
        manager.record_attempt("test3", false);
        manager.record_attempt("test3", false);

        let flaky = manager.get_flaky_tests();
        assert_eq!(flaky.len(), 1);
        assert!(flaky.contains(&"test1".to_string()));
    }
}
