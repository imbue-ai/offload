//! Tracks test completion for progress reporting and termination.
//!
//! A test has a **decided** outcome when it has passed/become flaky, or when
//! it has exhausted all retry attempts while still failing. The progress bar
//! and cancellation logic both use [`CompletionTracker::decided_count`].

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

/// Maps string test IDs to contiguous numeric indices.
pub type TestToIdxMap = indexmap::IndexSet<String>;

/// Numeric index for a test within the [`TestToIdxMap`].
pub type TestIdx = usize;

/// Numeric index for a batch, assigned atomically by the orchestrator.
pub type BatchIdx = usize;

/// Tracks which tests have a decided outcome.
///
/// Call [`newly_complete_tests`] after each batch returns to increment attempt
/// counts and update the decided set.
pub struct CompletionTracker {
    index: TestToIdxMap,
    max_attempts: Vec<usize>,
    attempt_counts: Vec<usize>,
    decided: Vec<bool>,
    decided_count: usize,
    total_expected: usize,
    incomplete: IncompleteTestsRegistry,
}

impl CompletionTracker {
    pub fn new(total_expected: usize, index: TestToIdxMap) -> Self {
        let len = index.len();
        Self {
            index,
            max_attempts: vec![1; len],
            attempt_counts: vec![0; len],
            decided: vec![false; len],
            decided_count: 0,
            total_expected,
            incomplete: IncompleteTestsRegistry::new(),
        }
    }

    /// Registers the maximum number of attempts for a test.
    pub fn register_retries(&mut self, test_id: &str, max_attempts: usize) {
        if let Some(idx) = self.index.get_index_of(test_id) {
            self.max_attempts[idx] = max_attempts;
        }
    }

    /// Records one attempt for each test in the batch and updates decided set.
    ///
    /// `is_passed` should return true if the test has passed or is flaky.
    /// Returns the numeric indices of tests that became newly decided.
    /// Also notifies the internal [`IncompleteTestsRegistry`] so that
    /// per-batch cancellation tokens fire when all tests are decided.
    pub fn newly_complete_tests(
        &mut self,
        test_ids: &[&str],
        is_passed: impl Fn(&str) -> bool,
    ) -> Vec<TestIdx> {
        let mut newly_decided = Vec::new();
        for &test_id in test_ids {
            let Some(num_id) = self.index.get_index_of(test_id) else {
                continue;
            };
            if self.decided[num_id] {
                continue;
            }

            self.attempt_counts[num_id] += 1;

            let is_now_decided = if is_passed(test_id) {
                true
            } else {
                self.attempt_counts[num_id] >= self.max_attempts[num_id]
            };

            if is_now_decided {
                self.decided[num_id] = true;
                self.decided_count += 1;
                newly_decided.push(num_id);
            }
        }
        for &num_id in &newly_decided {
            self.incomplete.notify_decided(num_id);
        }
        newly_decided
    }

    /// Registers a running batch for per-batch cancellation.
    ///
    /// Already-decided tests are filtered out. If all tests are already
    /// decided, the token is cancelled immediately.
    pub fn register_batch(
        &mut self,
        batch_idx: BatchIdx,
        test_ids: &[&str],
        token: CancellationToken,
    ) {
        let undecided: Vec<TestIdx> = test_ids
            .iter()
            .filter_map(|id| self.index.get_index_of(*id))
            .filter(|&idx| !self.decided[idx])
            .collect();

        if undecided.is_empty() {
            token.cancel();
            return;
        }

        self.incomplete.register(batch_idx, &undecided, token);
    }

    /// Returns true if every named test has a decided outcome.
    pub fn all_decided_by_name<'a>(&self, mut test_ids: impl Iterator<Item = &'a str>) -> bool {
        test_ids.all(|id| {
            self.index
                .get_index_of(id)
                .is_some_and(|idx| self.decided[idx])
        })
    }

    /// Number of tests with a decided outcome.
    pub fn decided_count(&self) -> usize {
        self.decided_count
    }

    /// True when every expected test has a decided outcome.
    pub fn all_complete(&self) -> bool {
        self.decided_count == self.total_expected
    }
}

/// Tracks incomplete tests per batch for per-batch cancellation.
///
/// Each batch has a remaining count of undecided tests and a cancellation
/// token. When `notify_decided` decrements the count to zero, the token
/// is cancelled so the sandbox can be reclaimed early.
struct IncompleteTestsRegistry {
    /// batch_idx -> (remaining undecided count, cancellation token)
    batches: HashMap<BatchIdx, (usize, CancellationToken)>,
    /// test_num_id -> list of batch indices containing this test
    test_to_batches: HashMap<TestIdx, Vec<BatchIdx>>,
}

impl IncompleteTestsRegistry {
    fn new() -> Self {
        Self {
            batches: HashMap::new(),
            test_to_batches: HashMap::new(),
        }
    }

    /// Registers a batch with its undecided test IDs.
    fn register(
        &mut self,
        batch_idx: BatchIdx,
        undecided_ids: &[TestIdx],
        token: CancellationToken,
    ) {
        self.batches.insert(batch_idx, (undecided_ids.len(), token));
        for &test_id in undecided_ids {
            self.test_to_batches
                .entry(test_id)
                .or_default()
                .push(batch_idx);
        }
    }

    /// Notifies the registry that a test has been decided.
    ///
    /// Decrements the remaining count for each batch containing this test.
    /// When a batch's count reaches zero, its token is cancelled.
    fn notify_decided(&mut self, test_num_id: TestIdx) {
        if let Some(batch_idxs) = self.test_to_batches.remove(&test_num_id) {
            for batch_idx in batch_idxs {
                if let Some((remaining, token)) = self.batches.get_mut(&batch_idx) {
                    *remaining = remaining.saturating_sub(1);
                    if *remaining == 0 {
                        tracing::info!(
                            "PER-BATCH CANCEL: Batch {} has all tests decided, cancelling",
                            batch_idx,
                        );
                        token.cancel();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_index(ids: &[&str]) -> TestToIdxMap {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_all_passed_immediately_decided() {
        let index = test_index(&["test_a", "test_b"]);
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 3);
        tracker.register_retries("test_b", 3);

        let _ = tracker.newly_complete_tests(&["test_a", "test_b"], |_| true);

        assert_eq!(tracker.decided_count(), 2);
        assert!(tracker.all_complete());
    }

    #[test]
    fn test_failure_with_retries_remaining() {
        let index = test_index(&["test_a", "test_b"]);
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 3);

        // test_a passes, test_b fails (has retries remaining)
        let _ = tracker.newly_complete_tests(&["test_a", "test_b"], |id| id == "test_a");

        assert_eq!(tracker.decided_count(), 1);
        assert!(!tracker.all_complete());
    }

    #[test]
    fn test_failure_retries_exhausted() {
        let index = test_index(&["test_a", "test_b"]);
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 2);

        // Attempt 1: test_a passes, test_b fails
        let _ = tracker.newly_complete_tests(&["test_a", "test_b"], |id| id == "test_a");
        assert_eq!(tracker.decided_count(), 1);

        // Attempt 2: test_b fails again, retries exhausted
        let _ = tracker.newly_complete_tests(&["test_b"], |_| false);
        assert_eq!(tracker.decided_count(), 2);
        assert!(tracker.all_complete());
    }

    #[test]
    fn test_missing_test_not_complete() {
        let index = test_index(&["test_a", "test_b"]);
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 1);

        let _ = tracker.newly_complete_tests(&["test_a"], |_| true);

        assert_eq!(tracker.decided_count(), 1);
        assert!(!tracker.all_complete());
    }

    #[test]
    fn test_already_decided_not_double_counted() {
        let index = test_index(&["test_a"]);
        let mut tracker = CompletionTracker::new(1, index);
        tracker.register_retries("test_a", 1);

        let _ = tracker.newly_complete_tests(&["test_a"], |_| true);
        let _ = tracker.newly_complete_tests(&["test_a"], |_| true); // duplicate

        assert_eq!(tracker.decided_count(), 1);
    }

    // --- IncompleteTestsRegistry tests ---

    #[test]
    fn test_registry_cancel_when_all_decided() {
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone());
        assert!(!token.is_cancelled());

        registry.notify_decided(0);
        assert!(!token.is_cancelled());

        registry.notify_decided(1);
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_registry_no_cancel_when_partial() {
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone());

        registry.notify_decided(0);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_register_batch_all_already_decided_cancels_immediately() {
        let index = test_index(&["test_a", "test_b"]);
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 1);

        // Decide all tests
        let _ = tracker.newly_complete_tests(&["test_a", "test_b"], |_| true);

        // Register a batch for already-decided tests
        let token = CancellationToken::new();
        tracker.register_batch(0, &["test_a", "test_b"], token.clone());
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_registry_counter_reaches_zero_cancels() {
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1, 2], token.clone());
        assert!(!token.is_cancelled());

        registry.notify_decided(0);
        assert!(!token.is_cancelled());

        registry.notify_decided(1);
        assert!(!token.is_cancelled());

        // Counter reaches zero, token is cancelled
        registry.notify_decided(2);
        assert!(token.is_cancelled());
    }
}
