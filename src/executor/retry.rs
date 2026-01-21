//! Retry and flakiness detection logic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Manages test retries and tracks flakiness.
#[derive(Clone)]
pub struct RetryManager {
    max_retries: usize,
    /// Tracks retry attempts per test: (attempts, successes)
    attempts: Arc<Mutex<HashMap<String, (usize, usize)>>>,
    /// Maximum tests to retry for various failure reasons
    #[allow(dead_code)]
    budget: RetryBudget,
}

/// Budget limits for different failure categories.
#[derive(Clone)]
pub struct RetryBudget {
    /// Max tests to retry for timeout failures
    pub timeout: usize,
    /// Max tests to retry for regular failures
    pub failed: usize,
    /// Max tests to retry for known exceptions
    pub known_errors: usize,
    /// Max tests to retry for unknown exceptions
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
    /// Create a new retry manager with the given max retries per test.
    pub fn new(max_retries: usize) -> Self {
        Self {
            max_retries,
            attempts: Arc::new(Mutex::new(HashMap::new())),
            budget: RetryBudget::default(),
        }
    }

    /// Create a new retry manager with custom budget.
    pub fn with_budget(max_retries: usize, budget: RetryBudget) -> Self {
        Self {
            max_retries,
            attempts: Arc::new(Mutex::new(HashMap::new())),
            budget,
        }
    }

    /// Get the maximum number of retries.
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// Check if a test should be retried.
    pub fn should_retry(&self, test_id: &str) -> bool {
        let attempts = self.attempts.lock().unwrap();
        let (count, _) = attempts.get(test_id).unwrap_or(&(0, 0));
        *count < self.max_retries
    }

    /// Record a retry attempt.
    pub fn record_attempt(&self, test_id: &str, success: bool) {
        let mut attempts = self.attempts.lock().unwrap();
        let entry = attempts.entry(test_id.to_string()).or_insert((0, 0));
        entry.0 += 1;
        if success {
            entry.1 += 1;
        }
    }

    /// Get the number of attempts for a test.
    pub fn get_attempts(&self, test_id: &str) -> usize {
        let attempts = self.attempts.lock().unwrap();
        attempts.get(test_id).map(|(c, _)| *c).unwrap_or(0)
    }

    /// Check if a test is flaky (passed after initial failure).
    pub fn is_flaky(&self, test_id: &str) -> bool {
        let attempts = self.attempts.lock().unwrap();
        if let Some((attempts, successes)) = attempts.get(test_id) {
            // Test is flaky if it had at least one failure and one success
            *attempts > 1 && *successes > 0 && *successes < *attempts
        } else {
            false
        }
    }

    /// Get all flaky test IDs.
    pub fn get_flaky_tests(&self) -> Vec<String> {
        let attempts = self.attempts.lock().unwrap();
        attempts
            .iter()
            .filter(|(_, (attempts, successes))| {
                *attempts > 1 && *successes > 0 && *successes < *attempts
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get retry statistics.
    pub fn stats(&self) -> RetryStats {
        let attempts = self.attempts.lock().unwrap();

        let total_tests = attempts.len();
        let total_retries: usize = attempts.values().map(|(c, _)| c.saturating_sub(1)).sum();
        let flaky_tests = attempts
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

/// Statistics about retry attempts.
#[derive(Debug, Clone)]
pub struct RetryStats {
    /// Total number of unique tests that were attempted.
    pub total_tests: usize,
    /// Total number of retry attempts (excluding first run).
    pub total_retries: usize,
    /// Number of tests identified as flaky.
    pub flaky_tests: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_manager_basic() {
        let manager = RetryManager::new(3);

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
        let manager = RetryManager::new(3);

        // Test that fails first, then passes
        manager.record_attempt("test1", false);
        manager.record_attempt("test1", true);

        assert!(manager.is_flaky("test1"));
    }

    #[test]
    fn test_not_flaky_if_always_passes() {
        let manager = RetryManager::new(3);

        manager.record_attempt("test1", true);
        manager.record_attempt("test1", true);

        assert!(!manager.is_flaky("test1"));
    }

    #[test]
    fn test_not_flaky_if_always_fails() {
        let manager = RetryManager::new(3);

        manager.record_attempt("test1", false);
        manager.record_attempt("test1", false);

        assert!(!manager.is_flaky("test1"));
    }

    #[test]
    fn test_get_flaky_tests() {
        let manager = RetryManager::new(3);

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
