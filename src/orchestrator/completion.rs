//! Tracks test completion for progress reporting and termination.
//!
//! A test has a **decided** outcome when it has passed/become flaky, or when
//! it has exhausted all retry attempts while still failing. The progress bar
//! and cancellation logic both use [`CompletionTracker::decided_count`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

/// Shared completion tracker, protected by a mutex for concurrent access.
pub type SharedCompletionTracker = Arc<Mutex<CompletionTracker>>;

/// Shared decided flags for lock-free decided-status checks.
pub type SharedDecidedFlags = Arc<DecidedFlags>;

/// Shared test index for mapping string test IDs to numeric indices.
pub type SharedTestIndex = Arc<TestIndex>;

/// Shared incomplete-tests registry, protected by a mutex for concurrent access.
pub type SharedIncompleteTestsRegistry = Arc<Mutex<IncompleteTestsRegistry>>;

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

    /// Returns true if this test has a decided outcome.
    pub fn is_decided(&self, test_id: &str) -> bool {
        self.decided.contains(test_id)
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

/// Maps string test IDs to compact `usize` indices for cache-friendly lookups.
pub struct TestIndex {
    id_to_idx: HashMap<String, usize>,
}

impl TestIndex {
    /// Builds an index from a slice of test ID strings, assigning contiguous
    /// indices. Duplicate IDs are deduplicated (they share the same index).
    pub fn new(test_ids: &[&str]) -> Self {
        let mut id_to_idx = HashMap::new();
        for &id in test_ids {
            let next = id_to_idx.len();
            id_to_idx.entry(id.to_string()).or_insert(next);
        }
        Self { id_to_idx }
    }

    /// Returns the numeric index for `test_id`, or `None` if unknown.
    pub fn get(&self, test_id: &str) -> Option<usize> {
        self.id_to_idx.get(test_id).copied()
    }

    /// Number of distinct test IDs in the index.
    pub fn len(&self) -> usize {
        self.id_to_idx.len()
    }

    /// Returns true if the index contains no test IDs.
    pub fn is_empty(&self) -> bool {
        self.id_to_idx.is_empty()
    }
}

/// Lock-free decided-status array indexed by numeric test ID.
///
/// Uses interior mutability via `AtomicBool` (pre-authorized by coordinator
/// for per-batch cancellation).
pub struct DecidedFlags {
    flags: Vec<AtomicBool>,
}

impl DecidedFlags {
    /// Creates a new flags array with `count` entries, all initially `false`.
    pub fn new(count: usize) -> Self {
        let flags = (0..count).map(|_| AtomicBool::new(false)).collect();
        Self { flags }
    }

    /// Marks the test at `idx` as decided. No-op if `idx` is out of bounds.
    pub fn mark_decided(&self, idx: usize) {
        if let Some(flag) = self.flags.get(idx) {
            flag.store(true, Ordering::Release);
        }
    }

    /// Returns whether the test at `idx` is decided. Returns `false` if out of bounds.
    pub fn is_decided(&self, idx: usize) -> bool {
        self.flags
            .get(idx)
            .is_some_and(|f| f.load(Ordering::Acquire))
    }
}

struct IncompleteEntry {
    remaining: usize,
    token: CancellationToken,
}

/// Tracks incomplete tests and enables efficient per-test cancellation notification.
///
/// When all tests in a batch become decided, the batch's cancellation token is
/// cancelled so the sandbox can be reclaimed early.
pub struct IncompleteTestsRegistry {
    entries: HashMap<usize, IncompleteEntry>,
    test_to_batches: HashMap<usize, Vec<usize>>,
}

impl Default for IncompleteTestsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IncompleteTestsRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            test_to_batches: HashMap::new(),
        }
    }

    /// Registers a batch with the given test numeric IDs.
    ///
    /// Already-decided tests (according to `decided`) are filtered out. If all
    /// tests are already decided, the token is cancelled immediately and the
    /// batch is not added to the registry.
    pub fn register(
        &mut self,
        batch_idx: usize,
        test_num_ids: &[usize],
        token: CancellationToken,
        decided: &DecidedFlags,
    ) {
        let undecided: Vec<usize> = test_num_ids
            .iter()
            .copied()
            .filter(|&id| !decided.is_decided(id))
            .collect();

        if undecided.is_empty() {
            token.cancel();
            return;
        }

        self.entries.insert(
            batch_idx,
            IncompleteEntry {
                remaining: undecided.len(),
                token,
            },
        );

        for &test_id in &undecided {
            self.test_to_batches
                .entry(test_id)
                .or_default()
                .push(batch_idx);
        }
    }

    /// Removes a batch from the registry. Stale reverse-index refs are harmless.
    pub fn unregister(&mut self, batch_idx: usize) {
        self.entries.remove(&batch_idx);
    }

    /// Notifies the registry that a test has been decided.
    ///
    /// Decrements the remaining count for each batch containing this test.
    /// When a batch's remaining count reaches zero, its token is cancelled.
    pub fn notify_decided(&mut self, test_num_id: usize) {
        if let Some(batch_idxs) = self.test_to_batches.remove(&test_num_id) {
            for batch_idx in batch_idxs {
                if let Some(entry) = self.entries.get_mut(&batch_idx) {
                    entry.remaining = entry.remaining.saturating_sub(1);
                    if entry.remaining == 0 {
                        entry.token.cancel();
                    }
                }
            }
        }
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

    // --- TestIndex tests ---

    #[test]
    fn test_index_basic_lookup() {
        let idx = TestIndex::new(&["a", "b", "c"]);
        assert_eq!(idx.len(), 3);
        assert!(idx.get("a").is_some());
        assert!(idx.get("b").is_some());
        assert!(idx.get("c").is_some());
        // All indices should be distinct
        let a = idx.get("a");
        let b = idx.get("b");
        let c = idx.get("c");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn test_index_unknown_returns_none() {
        let idx = TestIndex::new(&["a", "b"]);
        assert_eq!(idx.get("unknown"), None);
    }

    #[test]
    fn test_index_dedup() {
        let idx = TestIndex::new(&["a", "b", "a"]);
        assert_eq!(idx.len(), 2);
        assert!(idx.get("a").is_some());
        assert!(idx.get("b").is_some());
    }

    // --- DecidedFlags tests ---

    #[test]
    fn test_decided_flags_initially_false() {
        let flags = DecidedFlags::new(3);
        assert!(!flags.is_decided(0));
        assert!(!flags.is_decided(1));
        assert!(!flags.is_decided(2));
    }

    #[test]
    fn test_decided_flags_mark_and_check() {
        let flags = DecidedFlags::new(3);
        flags.mark_decided(1);
        assert!(flags.is_decided(1));
        assert!(!flags.is_decided(0));
    }

    #[test]
    fn test_decided_flags_out_of_bounds() {
        let flags = DecidedFlags::new(3);
        flags.mark_decided(999); // no-op
        assert!(!flags.is_decided(999));
    }

    // --- IncompleteTestsRegistry tests ---

    #[test]
    fn test_registry_cancel_when_all_decided() {
        let decided = DecidedFlags::new(2);
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone(), &decided);
        assert!(!token.is_cancelled());

        registry.notify_decided(0);
        assert!(!token.is_cancelled());

        registry.notify_decided(1);
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_registry_no_cancel_when_partial() {
        let decided = DecidedFlags::new(2);
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone(), &decided);

        registry.notify_decided(0);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_registry_register_all_already_decided() {
        let decided = DecidedFlags::new(2);
        decided.mark_decided(0);
        decided.mark_decided(1);

        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone(), &decided);
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_registry_unregister_removes_entry() {
        let decided = DecidedFlags::new(2);
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone(), &decided);
        registry.unregister(0);

        // notify_decided for removed batch should not panic or cancel
        registry.notify_decided(0);
        registry.notify_decided(1);
        assert!(!token.is_cancelled());
    }
}
