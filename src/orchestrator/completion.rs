//! Tracks test completion for progress reporting and termination.
//!
//! A test has a **decided** outcome when it has passed/become flaky, or when
//! it has exhausted all retry attempts while still failing. The progress bar
//! and cancellation logic both use [`CompletionTracker::decided_count`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

/// Tracks which tests have a decided outcome.
///
/// Call [`newly_complete_tests`] after each batch returns to increment attempt
/// counts and update the decided set. Owns [`DecidedFlags`] internally so that
/// lock-free decided-status checks stay in sync with the authoritative state.
pub struct CompletionTracker {
    index: Arc<TestIndex>,
    max_attempts: Vec<usize>,
    attempt_counts: Vec<usize>,
    decided: Arc<DecidedFlags>,
    decided_count: usize,
    total_expected: usize,
    incomplete: IncompleteTestsRegistry,
}

impl CompletionTracker {
    pub fn new(total_expected: usize, index: Arc<TestIndex>) -> Self {
        let len = index.len();
        let decided = Arc::new(DecidedFlags::new(len));
        Self {
            index,
            max_attempts: vec![1; len], // default 1 attempt
            attempt_counts: vec![0; len],
            decided,
            decided_count: 0,
            total_expected,
            incomplete: IncompleteTestsRegistry::new(),
        }
    }

    /// Registers the maximum number of attempts for a test.
    pub fn register_retries(&mut self, test_id: &str, max_attempts: usize) {
        if let Some(idx) = self.index.get(test_id) {
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
    ) -> Vec<usize> {
        let mut newly_decided = Vec::new();
        for &test_id in test_ids {
            let Some(num_id) = self.index.get(test_id) else {
                continue;
            };
            if self.decided.is_decided(num_id) {
                continue;
            }

            self.attempt_counts[num_id] += 1;

            let is_now_decided = if is_passed(test_id) {
                true
            } else {
                self.attempt_counts[num_id] >= self.max_attempts[num_id]
            };

            if is_now_decided {
                self.decided.mark_decided(num_id);
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
        batch_idx: usize,
        test_num_ids: &[usize],
        token: CancellationToken,
    ) {
        self.incomplete
            .register(batch_idx, test_num_ids, token, &self.decided);
    }

    /// Number of tests with a decided outcome.
    pub fn decided_count(&self) -> usize {
        self.decided_count
    }

    /// True when every expected test has a decided outcome.
    pub fn all_complete(&self) -> bool {
        self.decided_count == self.total_expected
    }

    /// Returns the shared decided flags for lock-free access.
    pub fn decided_flags(&self) -> Arc<DecidedFlags> {
        Arc::clone(&self.decided)
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

/// Cancels a batch's token when the last reference is dropped.
///
/// Shared via `Arc` between all incomplete tests in the batch. When the
/// last test is decided and removed from the registry, the final clone
/// drops and cancellation fires automatically.
///
/// Uses `Arc` (not `Rc`) because `CompletionTracker` is shared across worker
/// tasks via `Arc<Mutex<...>>`, which requires the inner type to be `Send`.
/// `Rc` is `!Send`. The atomic overhead is negligible since all access is
/// serialized by the outer `Mutex`.
struct BatchGuard {
    batch_idx: usize,
    token: CancellationToken,
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        if !self.token.is_cancelled() {
            tracing::info!(
                "PER-BATCH CANCEL: Batch {} has all tests decided, cancelling",
                self.batch_idx,
            );
        }
        self.token.cancel();
    }
}

/// Tracks incomplete tests and enables efficient per-test cancellation notification.
///
/// When all tests in a batch become decided, the batch's cancellation token is
/// cancelled so the sandbox can be reclaimed early. This is achieved via
/// `Arc<BatchGuard>`: each undecided test holds a clone, and when the last
/// clone is dropped the `Drop` impl fires cancellation.
struct IncompleteTestsRegistry {
    test_to_guards: HashMap<usize, Vec<Arc<BatchGuard>>>,
}

impl IncompleteTestsRegistry {
    fn new() -> Self {
        Self {
            test_to_guards: HashMap::new(),
        }
    }

    /// Registers a batch with the given test numeric IDs.
    ///
    /// Already-decided tests (according to `decided`) are filtered out. If all
    /// tests are already decided, the token is cancelled immediately.
    fn register(
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

        let guard = Arc::new(BatchGuard { batch_idx, token });

        for &test_id in &undecided {
            self.test_to_guards
                .entry(test_id)
                .or_default()
                .push(Arc::clone(&guard));
        }
    }

    /// Notifies the registry that a test has been decided.
    ///
    /// Removes all guard references for this test. When a guard's last
    /// reference is dropped, its `Drop` impl cancels the batch token.
    fn notify_decided(&mut self, test_num_id: usize) {
        self.test_to_guards.remove(&test_num_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_passed_immediately_decided() {
        let index = Arc::new(TestIndex::new(&["test_a", "test_b"]));
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 3);
        tracker.register_retries("test_b", 3);

        let _ = tracker.newly_complete_tests(&["test_a", "test_b"], |_| true);

        assert_eq!(tracker.decided_count(), 2);
        assert!(tracker.all_complete());
    }

    #[test]
    fn test_failure_with_retries_remaining() {
        let index = Arc::new(TestIndex::new(&["test_a", "test_b"]));
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
        let index = Arc::new(TestIndex::new(&["test_a", "test_b"]));
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
        let index = Arc::new(TestIndex::new(&["test_a", "test_b"]));
        let mut tracker = CompletionTracker::new(2, index);
        tracker.register_retries("test_a", 1);
        tracker.register_retries("test_b", 1);

        let _ = tracker.newly_complete_tests(&["test_a"], |_| true);

        assert_eq!(tracker.decided_count(), 1);
        assert!(!tracker.all_complete());
    }

    #[test]
    fn test_already_decided_not_double_counted() {
        let index = Arc::new(TestIndex::new(&["test_a"]));
        let mut tracker = CompletionTracker::new(1, index);
        tracker.register_retries("test_a", 1);

        let _ = tracker.newly_complete_tests(&["test_a"], |_| true);
        let _ = tracker.newly_complete_tests(&["test_a"], |_| true); // duplicate

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
    fn test_registry_stale_guards_harmless() {
        let decided = DecidedFlags::new(2);
        let mut registry = IncompleteTestsRegistry::new();
        let token = CancellationToken::new();

        registry.register(0, &[0, 1], token.clone(), &decided);

        // Deciding all tests drops the last guard and cancels the token
        registry.notify_decided(0);
        registry.notify_decided(1);
        assert!(token.is_cancelled());

        // Cancelling an already-cancelled token is a no-op
        token.cancel();
        assert!(token.is_cancelled());
    }
}
