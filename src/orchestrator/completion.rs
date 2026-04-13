//! Tracks test completion for progress reporting and termination.
//!
//! A test has a **decided** outcome when it has passed/become flaky, or when
//! it has exhausted all retry attempts while still failing. The progress bar
//! and cancellation logic both use [`CompletionTracker::decided_count`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Shared completion tracker, protected by a mutex for concurrent access.
pub type SharedCompletionTracker = Arc<Mutex<CompletionTracker>>;

/// Tracks which tests have a decided outcome.
///
/// Call [`record_batch`] after each batch returns to increment attempt counts
/// and update the decided set.
pub struct CompletionTracker {
    max_attempts: HashMap<String, usize>,
    attempt_counts: HashMap<String, usize>,
    decided: HashSet<String>,
    total_expected: usize,
}

impl CompletionTracker {
    pub fn new(total_expected: usize) -> Self {
        Self {
            max_attempts: HashMap::new(),
            attempt_counts: HashMap::new(),
            decided: HashSet::new(),
            total_expected,
        }
    }

    /// Registers the maximum number of attempts for a test.
    pub fn register_retries(&mut self, test_id: &str, max_attempts: usize) {
        self.max_attempts.insert(test_id.to_string(), max_attempts);
    }

    /// Records one attempt for each test in the batch and updates decided set.
    ///
    /// `is_passed` should return true if the test has passed or is flaky.
    pub fn record_batch(&mut self, test_ids: &[&str], is_passed: impl Fn(&str) -> bool) {
        for &test_id in test_ids {
            if self.decided.contains(test_id) {
                continue;
            }

            *self.attempt_counts.entry(test_id.to_string()).or_insert(0) += 1;

            let decided = if is_passed(test_id) {
                true
            } else {
                let attempts = self.attempt_counts[test_id];
                let max = self.max_attempts.get(test_id).copied().unwrap_or(1);
                attempts >= max
            };

            if decided {
                self.decided.insert(test_id.to_string());
            }
        }
    }

    /// Number of tests with a decided outcome.
    pub fn decided_count(&self) -> usize {
        self.decided.len()
    }

    /// True when every expected test has a decided outcome.
    pub fn all_complete(&self) -> bool {
        self.decided_count() == self.total_expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_passed_immediately_decided() {
        let mut tracker = CompletionTracker::new(2);
        tracker.register_retries("test_a", 3);
        tracker.register_retries("test_b", 3);

        tracker.record_batch(&["test_a", "test_b"], |_| true);

        assert_eq!(tracker.decided_count(), 2);
        assert!(tracker.all_complete());
    }

    #[test]
    fn test_failure_with_retries_remaining() {
        let mut tracker = CompletionTracker::new(2);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 3);

        // test_a passes, test_b fails (has retries remaining)
        tracker.record_batch(&["test_a", "test_b"], |id| id == "test_a");

        assert_eq!(tracker.decided_count(), 1);
        assert!(!tracker.all_complete());
    }

    #[test]
    fn test_failure_retries_exhausted() {
        let mut tracker = CompletionTracker::new(2);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 2);

        // Attempt 1: test_a passes, test_b fails
        tracker.record_batch(&["test_a", "test_b"], |id| id == "test_a");
        assert_eq!(tracker.decided_count(), 1);

        // Attempt 2: test_b fails again, retries exhausted
        tracker.record_batch(&["test_b"], |_| false);
        assert_eq!(tracker.decided_count(), 2);
        assert!(tracker.all_complete());
    }

    #[test]
    fn test_missing_test_not_complete() {
        let mut tracker = CompletionTracker::new(2);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 1);

        tracker.record_batch(&["test_a"], |_| true);

        assert_eq!(tracker.decided_count(), 1);
        assert!(!tracker.all_complete());
    }

    #[test]
    fn test_already_decided_not_double_counted() {
        let mut tracker = CompletionTracker::new(1);
        tracker.register_retries("test_a", 1);

        tracker.record_batch(&["test_a"], |_| true);
        tracker.record_batch(&["test_a"], |_| true); // duplicate

        assert_eq!(tracker.decided_count(), 1);
    }
}
