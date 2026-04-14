//! Test scheduling and distribution across parallel sandboxes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::framework::TestInstance;

/// A scheduled batch of tests ready for execution.
///
/// Wraps a list of test instances along with their combined estimated duration.
/// Workers pop these from the scheduler queue and pass them to `register_running_batch`.
#[derive(Clone)]
pub struct ScheduledBatch {
    pub tests: Vec<TestInstance>,
    estimated_load: Duration,
}

impl ScheduledBatch {
    /// Total estimated duration for all tests in this batch.
    pub fn estimated_load(&self) -> Duration {
        self.estimated_load
    }
}

/// Maximum total length (in chars) of all test IDs in a single batch.
///
/// Prevents command lines from exceeding OS or shell limits. A single test
/// whose ID already exceeds this is still placed alone in its own batch.
const MAX_BATCH_COMMAND_LEN: usize = 30_000;

/// A batch of tests being built by the scheduler.
///
/// Tracks the tests, their cumulative expected duration, total command length,
/// and which test IDs are present (to prevent scheduling the same test twice
/// in one batch).
struct Batch {
    tests: Vec<TestInstance>,
    load: Duration,
    command_len: usize,
    test_ids: HashSet<String>,
}

impl Batch {
    fn new() -> Self {
        Self {
            tests: Vec::new(),
            load: Duration::ZERO,
            command_len: 0,
            test_ids: HashSet::new(),
        }
    }

    fn add(&mut self, test: TestInstance, duration: Duration) {
        self.command_len += test.id().len();
        self.test_ids.insert(test.id().to_string());
        self.tests.push(test);
        self.load += duration;
    }

    fn contains(&self, test_id: &str) -> bool {
        self.test_ids.contains(test_id)
    }

    fn is_empty(&self) -> bool {
        self.tests.is_empty()
    }

    fn would_fit(
        &self,
        test_id_len: usize,
        duration: Duration,
        max_batch_duration: Option<Duration>,
    ) -> bool {
        self.is_empty()
            || (self.command_len + test_id_len <= MAX_BATCH_COMMAND_LEN
                && max_batch_duration.is_none_or(|cap| self.load + duration <= cap))
    }
}

/// Distributes tests across parallel sandboxes.
///
/// Performs LPT scheduling at construction time and exposes the resulting
/// batches through a mutex-protected queue. Workers call [`pop`](Self::pop)
/// to pull batches. The `register_running_batch` method provides hedged re-queuing when
/// a batch takes too long.
pub struct Scheduler {
    queue: Mutex<VecDeque<ScheduledBatch>>,
    notify: tokio::sync::Notify,
    batch_count: usize,
    batch_sizes: Vec<usize>,
}

impl Scheduler {
    /// Creates a new scheduler and schedules the given tests.
    ///
    /// Uses Longest Processing Time First (LPT) algorithm with historical
    /// test durations to minimize total execution time (makespan). Tests are
    /// sorted by duration descending and assigned to the worker with the
    /// smallest current total workload.
    ///
    /// Batches are sorted by total duration descending, so the heaviest batch
    /// is first. This ensures it gets scheduled first with Modal.
    ///
    /// When `max_batch_duration` is set, batches that would exceed the cap are
    /// not eligible for assignment, and new batches are created as needed. This
    /// means the total number of batches may exceed `max_parallel`.
    ///
    /// # Arguments
    ///
    /// * `max_parallel` - Maximum number of parallel batches/sandboxes.
    ///   Minimum is 1 (values below 1 are clamped).
    /// * `tests` - Tests to schedule
    /// * `durations` - Historical test durations from previous runs.
    ///   Tests not in the map use the per-group average from `group_to_default_duration`.
    /// * `group_to_default_duration` - Per-group average duration for tests without historical data.
    ///   Falls back to 1 second if the group has no entry.
    /// * `max_batch_duration` - Optional cap on the total duration of each batch.
    ///   A single test that exceeds the cap is still placed alone in its own batch.
    pub fn new(
        max_parallel: usize,
        tests: &[TestInstance],
        durations: &HashMap<String, Duration>,
        group_to_default_duration: &HashMap<String, Duration>,
        max_batch_duration: Option<Duration>,
    ) -> Self {
        let max_parallel = max_parallel.max(1);

        if tests.is_empty() {
            return Self {
                queue: Mutex::new(VecDeque::new()),
                notify: tokio::sync::Notify::new(),
                batch_count: 0,
                batch_sizes: Vec::new(),
            };
        }

        // Partition: individually-scheduled tests get their own batches, others go through LPT
        let (individual_tests, normal_tests): (Vec<_>, Vec<_>) =
            tests.iter().cloned().partition(|t| t.schedule_individual());

        // Build individual batches (one test per batch) with estimated load
        let individual_batches: Vec<ScheduledBatch> = individual_tests
            .into_iter()
            .map(|t| {
                let load = durations
                    .get(t.id())
                    .copied()
                    .or_else(|| group_to_default_duration.get(t.group()).copied())
                    .unwrap_or(Duration::from_secs(1));
                ScheduledBatch {
                    tests: vec![t],
                    estimated_load: load,
                }
            })
            .collect();

        if normal_tests.is_empty() {
            let batch_count = individual_batches.len();
            let batch_sizes = individual_batches.iter().map(|b| b.tests.len()).collect();
            return Self {
                queue: Mutex::new(VecDeque::from(individual_batches)),
                notify: tokio::sync::Notify::new(),
                batch_count,
                batch_sizes,
            };
        }

        // Look up durations for each test, sorted longest-first
        let mut tests_with_duration: Vec<_> = normal_tests
            .iter()
            .map(|t| {
                let duration = match durations.get(t.id()) {
                    Some(&d) => d,
                    None => {
                        let fallback = group_to_default_duration
                            .get(t.group())
                            .copied()
                            .unwrap_or(Duration::from_secs(1));
                        tracing::debug!(
                            "No historical duration for test '{}', using group '{}' default {:?}",
                            t.id(),
                            t.group(),
                            fallback,
                        );
                        fallback
                    }
                };
                (t.clone(), duration)
            })
            .collect();
        tests_with_duration.sort_by(|a, b| b.1.cmp(&a.1));

        // Initialize batches
        let num_batches = max_parallel.min(normal_tests.len());
        let mut batches: Vec<Batch> = (0..num_batches).map(|_| Batch::new()).collect();

        // LPT assignment: assign each test to the lightest eligible batch
        for (test, duration) in tests_with_duration {
            let test_id = test.id();

            let target_idx = (0..batches.len())
                .filter(|&i| {
                    !batches[i].contains(test_id)
                        && batches[i].would_fit(test_id.len(), duration, max_batch_duration)
                })
                .min_by_key(|&i| batches[i].load);

            let idx = target_idx.unwrap_or_else(|| {
                batches.push(Batch::new());
                batches.len() - 1
            });

            batches[idx].add(test, duration);
        }

        // Sort by load descending (heaviest first) for optimal Modal scheduling
        batches.sort_by(|a, b| b.load.cmp(&a.load));

        // Prepend individually-scheduled batches before LPT batches
        let mut result = individual_batches;
        result.extend(
            batches
                .into_iter()
                .filter(|b| !b.is_empty())
                .map(|b| ScheduledBatch {
                    estimated_load: b.load,
                    tests: b.tests,
                }),
        );

        let batch_count = result.len();
        let batch_sizes = result.iter().map(|b| b.tests.len()).collect();
        Self {
            queue: Mutex::new(VecDeque::from(result)),
            notify: tokio::sync::Notify::new(),
            batch_count,
            batch_sizes,
        }
    }

    /// Removes and returns the next batch from the queue, blocking if empty.
    ///
    /// Waits for a batch to become available via [`push`](Self::push).
    /// Returns `None` if the mutex is poisoned.
    ///
    /// This future is cancel-safe: callers should race it against a
    /// cancellation token via `tokio::select!`.
    pub async fn pop(&self) -> Option<ScheduledBatch> {
        loop {
            // Register notified BEFORE checking the queue to avoid missed wakeups
            let notified = self.notify.notified();

            match self.queue.lock() {
                Ok(mut q) => {
                    if let Some(batch) = q.pop_front() {
                        return Some(batch);
                    }
                }
                Err(e) => {
                    tracing::error!("batch queue mutex poisoned: {}", e);
                    return None;
                }
            }

            // Queue is empty — wait for push()
            notified.await;
        }
    }

    /// Pushes a batch to the back of the queue.
    fn push(&self, batch: ScheduledBatch) {
        match self.queue.lock() {
            Ok(mut q) => {
                q.push_back(batch);
                self.notify.notify_one();
            }
            Err(e) => {
                tracing::error!("batch queue mutex poisoned on push: {}", e);
            }
        }
    }

    /// Runs a future for a batch with hedged re-queuing.
    ///
    /// Computes a timeout of `2 * batch.estimated_load()`. If the future
    /// completes before that deadline, its result is returned immediately.
    /// If the timeout fires, the batch is split (or cloned if single-test)
    /// and re-queued, then the original future is awaited to completion.
    pub async fn register_running_batch<Fut, R>(&self, batch: &ScheduledBatch, fut: Fut) -> R
    where
        Fut: Future<Output = R>,
    {
        let requeue_after = 2 * batch.estimated_load();

        tokio::pin!(fut);

        match tokio::time::timeout(requeue_after, &mut fut).await {
            Ok(result) => result,
            Err(_) => {
                tracing::info!(
                    "Batch of {} tests exceeded requeue threshold ({:?}); re-queuing",
                    batch.tests.len(),
                    requeue_after,
                );

                if batch.tests.len() > 1 {
                    let mid = batch.tests.len() / 2;
                    let first_load = batch.estimated_load * mid as u32 / batch.tests.len() as u32;
                    let second_load = batch.estimated_load - first_load;

                    self.push(ScheduledBatch {
                        tests: batch.tests[..mid].to_vec(),
                        estimated_load: first_load,
                    });
                    self.push(ScheduledBatch {
                        tests: batch.tests[mid..].to_vec(),
                        estimated_load: second_load,
                    });
                } else {
                    self.push(batch.clone());
                }

                fut.await
            }
        }
    }

    /// Number of batches created during scheduling.
    pub fn batch_count(&self) -> usize {
        self.batch_count
    }

    /// Number of tests in each batch, in schedule order.
    pub fn batch_sizes(&self) -> &[usize] {
        &self.batch_sizes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::TestRecord;

    const MAX_BATCH_DURATION: Duration = Duration::from_secs(10);

    async fn drain_batches(scheduler: &Scheduler) -> Vec<Vec<TestInstance>> {
        let mut batches = Vec::new();
        while let Ok(Some(batch)) =
            tokio::time::timeout(Duration::from_millis(10), scheduler.pop()).await
        {
            batches.push(batch.tests);
        }
        batches
    }

    #[test]
    fn test_schedule_empty() {
        let scheduler = Scheduler::new(4, &[], &HashMap::new(), &HashMap::new(), None);
        assert_eq!(scheduler.batch_count(), 0);
    }

    #[tokio::test]
    async fn test_schedule_balances_load() {
        let records = [
            TestRecord::new("slow_test", "test-group"),
            TestRecord::new("medium_test", "test-group"),
            TestRecord::new("fast_test", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("slow_test".to_string(), Duration::from_secs(10));
        durations.insert("medium_test".to_string(), Duration::from_secs(5));
        durations.insert("fast_test".to_string(), Duration::from_secs(1));

        let scheduler = Scheduler::new(2, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // With LPT:
        // 1. Assign slow_test (10s) to worker 0 -> loads: [10, 0]
        // 2. Assign medium_test (5s) to worker 1 -> loads: [10, 5]
        // 3. Assign fast_test (1s) to worker 1 -> loads: [10, 6]
        // Batches sorted by load: batch 0 (10s), batch 1 (6s)
        assert_eq!(batches.len(), 2);
        // Heaviest batch first
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[0][0].id(), "slow_test");
        // Second batch has medium and fast
        assert_eq!(batches[1].len(), 2);
    }

    #[tokio::test]
    async fn test_schedule_heaviest_batch_first() {
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_b", "test-group"),
            TestRecord::new("test_c", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(1));
        durations.insert("test_b".to_string(), Duration::from_secs(5));
        durations.insert("test_c".to_string(), Duration::from_secs(3));

        let scheduler = Scheduler::new(3, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Each test in its own batch (3 workers, 3 tests)
        // Sorted by duration: test_b (5s), test_c (3s), test_a (1s)
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0][0].id(), "test_b"); // Heaviest first
        assert_eq!(batches[1][0].id(), "test_c");
        assert_eq!(batches[2][0].id(), "test_a");
    }

    #[tokio::test]
    async fn test_schedule_uses_default_for_unknown() {
        let records = [
            TestRecord::new("known_slow", "test-group"),
            TestRecord::new("unknown_test", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("known_slow".to_string(), Duration::from_secs(10));
        // unknown_test will use default of 1 second

        let scheduler = Scheduler::new(2, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        assert_eq!(batches.len(), 2);
        // known_slow (10s) should be in heaviest batch
        assert_eq!(batches[0][0].id(), "known_slow");
        assert_eq!(batches[1][0].id(), "unknown_test");
    }

    #[tokio::test]
    async fn test_schedule_duplicate_prevention() {
        // Simulate retry scenario: same test appears multiple times
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"), // retry 1
            TestRecord::new("test_a", "test-group"), // retry 2
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(5));

        let scheduler = Scheduler::new(3, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Each instance of test_a must be in a different batch
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
        assert_eq!(batches[2].len(), 1);

        // Verify no batch contains duplicate test IDs
        for batch in &batches {
            let ids: Vec<_> = batch.iter().map(|t| t.id()).collect();
            let unique: std::collections::HashSet<_> = ids.iter().collect();
            assert_eq!(ids.len(), unique.len(), "Batch contains duplicate test IDs");
        }
    }

    #[tokio::test]
    async fn test_schedule_mixed_duplicates_and_unique() {
        // Mix of retried and unique tests
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"), // retry
            TestRecord::new("test_b", "test-group"),
            TestRecord::new("test_c", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(10));
        durations.insert("test_b".to_string(), Duration::from_secs(5));
        durations.insert("test_c".to_string(), Duration::from_secs(1));

        let scheduler = Scheduler::new(3, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Verify no batch contains duplicate test IDs
        for batch in &batches {
            let ids: Vec<_> = batch.iter().map(|t| t.id()).collect();
            let unique: std::collections::HashSet<_> = ids.iter().collect();
            assert_eq!(ids.len(), unique.len(), "Batch contains duplicate test IDs");
        }

        // Both instances of test_a should exist across batches
        let all_ids: Vec<_> = batches
            .iter()
            .flat_map(|b| b.iter().map(|t| t.id()))
            .collect();
        assert_eq!(all_ids.iter().filter(|&&id| id == "test_a").count(), 2);
    }

    #[tokio::test]
    async fn test_schedule_creates_extra_batches_for_retries() {
        // 2 workers but 3 instances of same test — creates 3 batches (one per instance)
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let durations = HashMap::new();
        let scheduler = Scheduler::new(2, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Each instance must be in a separate batch
        assert_eq!(batches.len(), 3);
        for batch in &batches {
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].id(), "test_a");
        }
    }

    #[tokio::test]
    async fn test_schedule_respects_max_batch_duration() {
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_b", "test-group"),
            TestRecord::new("test_c", "test-group"),
            TestRecord::new("test_d", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(6));
        durations.insert("test_b".to_string(), Duration::from_secs(6));
        durations.insert("test_c".to_string(), Duration::from_secs(3));
        durations.insert("test_d".to_string(), Duration::from_secs(3));

        // With 10s cap: test_a (6s) + test_c (3s) = 9s OK, test_b (6s) + test_d (3s) = 9s OK
        let scheduler = Scheduler::new(
            2,
            &tests,
            &durations,
            &HashMap::new(),
            Some(MAX_BATCH_DURATION),
        );
        let batches = drain_batches(&scheduler).await;

        // Each batch total should be <= MAX_BATCH_DURATION
        for batch in &batches {
            let total: Duration = batch
                .iter()
                .map(|t| {
                    durations
                        .get(t.id())
                        .copied()
                        .unwrap_or(Duration::from_secs(1))
                })
                .sum();
            assert!(
                total <= MAX_BATCH_DURATION,
                "Batch duration {total:?} exceeds cap"
            );
        }
    }

    #[tokio::test]
    async fn test_schedule_long_test_gets_own_batch() -> anyhow::Result<()> {
        let records = [
            TestRecord::new("slow_test", "test-group"),
            TestRecord::new("fast_test", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("slow_test".to_string(), Duration::from_secs(15));
        durations.insert("fast_test".to_string(), Duration::from_secs(2));

        // slow_test exceeds the cap on its own, but that's fine — single test in batch
        let scheduler = Scheduler::new(
            2,
            &tests,
            &durations,
            &HashMap::new(),
            Some(MAX_BATCH_DURATION),
        );
        let batches = drain_batches(&scheduler).await;

        assert_eq!(batches.len(), 2);
        // slow_test should be alone in its batch
        let slow_batch = batches
            .iter()
            .find(|b| b.iter().any(|t| t.id() == "slow_test"))
            .ok_or_else(|| anyhow::anyhow!("slow_test batch not found"))?;
        assert_eq!(slow_batch.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_schedule_creates_extra_batches_for_duration_cap() {
        // 5 tests of 3s each, max_parallel=2, cap=10s
        // Can fit 3 tests per batch (9s < 10s), so need at least 2 batches
        // But only 2 workers, so tests get split: batch 0 = 3 tests (9s), batch 1 = 2 tests (6s)
        let records: Vec<_> = (0..7)
            .map(|i| TestRecord::new(format!("test_{}", i), "test-group"))
            .collect();
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        for i in 0..7 {
            durations.insert(format!("test_{}", i), Duration::from_secs(4));
        }

        // 7 tests * 4s = 28s total. Cap 10s means max 2 tests per batch (8s).
        // With 2 initial workers, need at least 4 batches (7 tests / 2 per batch)
        let scheduler = Scheduler::new(
            2,
            &tests,
            &durations,
            &HashMap::new(),
            Some(MAX_BATCH_DURATION),
        );
        let batches = drain_batches(&scheduler).await;

        assert!(
            batches.len() > 2,
            "Should create more batches than max_parallel"
        );
        for batch in &batches {
            let total: Duration = batch
                .iter()
                .map(|t| {
                    durations
                        .get(t.id())
                        .copied()
                        .unwrap_or(Duration::from_secs(1))
                })
                .sum();
            assert!(
                total <= MAX_BATCH_DURATION,
                "Batch duration {total:?} exceeds cap"
            );
        }
    }

    #[tokio::test]
    async fn test_schedule_splits_on_command_length() {
        // Create tests whose IDs together exceed MAX_BATCH_COMMAND_LEN
        let long_name = "a".repeat(MAX_BATCH_COMMAND_LEN / 2 + 1);
        let records = [
            TestRecord::new(format!("{long_name}_1"), "test-group"),
            TestRecord::new(format!("{long_name}_2"), "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let scheduler = Scheduler::new(1, &tests, &HashMap::new(), &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Two tests that each use >half the command length budget must be in separate batches
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[tokio::test]
    async fn test_schedule_groups_short_commands() {
        // Create many tests with short IDs that fit in one batch
        let records: Vec<_> = (0..100)
            .map(|i| TestRecord::new(format!("t{i}"), "test-group"))
            .collect();
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let scheduler = Scheduler::new(1, &tests, &HashMap::new(), &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Total command length is ~400 chars, well under 30k — should be 1 batch
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 100);
    }

    #[tokio::test]
    async fn test_schedule_individual_tests_get_own_batch() {
        let mut records = [
            TestRecord::new("fast_1", "fast-group"),
            TestRecord::new("fast_2", "fast-group"),
            TestRecord::new("slow_1", "slow-group"),
            TestRecord::new("slow_2", "slow-group"),
        ];
        records[2].schedule_individual = true;
        records[3].schedule_individual = true;

        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let durations = HashMap::new();
        let scheduler = Scheduler::new(2, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Each individually-scheduled test must be alone in its batch
        for batch in &batches {
            let has_individual = batch.iter().any(|t| t.schedule_individual());
            if has_individual {
                assert_eq!(
                    batch.len(),
                    1,
                    "Individually-scheduled test must be alone in its batch"
                );
            }
        }
        // All 4 tests should be scheduled
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, 4);
    }

    #[tokio::test]
    async fn test_schedule_individual_tests_preserves_interleaved_order() {
        // Simulate already-interleaved individual instances (as orchestrator would produce)
        let mut records = vec![
            TestRecord::new("slow_a", "slow-group"),
            TestRecord::new("slow_b", "slow-group"),
            TestRecord::new("slow_a", "slow-group"),
            TestRecord::new("slow_b", "slow-group"),
            TestRecord::new("slow_a", "slow-group"),
        ];
        for r in &mut records {
            r.schedule_individual = true;
        }

        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let durations = HashMap::new();
        let scheduler = Scheduler::new(4, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Each individually-scheduled test in its own batch, order preserved
        let ids: Vec<&str> = batches
            .iter()
            .map(|b| {
                assert_eq!(b.len(), 1);
                b[0].id()
            })
            .collect();
        assert_eq!(ids, vec!["slow_a", "slow_b", "slow_a", "slow_b", "slow_a"]);
    }

    #[tokio::test]
    async fn test_schedule_individual_tests_at_front() {
        let mut records = [
            TestRecord::new("fast_1", "fast-group"),
            TestRecord::new("slow_1", "slow-group"),
            TestRecord::new("fast_2", "fast-group"),
        ];
        records[1].schedule_individual = true;

        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let durations = HashMap::new();
        let scheduler = Scheduler::new(4, &tests, &durations, &HashMap::new(), None);
        let batches = drain_batches(&scheduler).await;

        // Individually-scheduled batches come first
        assert!(
            batches[0].iter().any(|t| t.schedule_individual()),
            "First batch should contain individually-scheduled test"
        );
        assert_eq!(batches[0].len(), 1);

        // Total tests scheduled
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn test_pop_returns_scheduled_batch_with_correct_estimated_load() -> anyhow::Result<()> {
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_b", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(3));
        durations.insert("test_b".to_string(), Duration::from_secs(7));

        // 1 worker => both tests in one batch with load = 3 + 7 = 10s
        let scheduler = Scheduler::new(1, &tests, &durations, &HashMap::new(), None);

        let batch = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for batch"))?
            .ok_or_else(|| anyhow::anyhow!("expected a batch"))?;

        assert_eq!(batch.estimated_load(), Duration::from_secs(10));
        assert_eq!(batch.tests.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_register_running_batch_completes_normally_before_threshold() -> anyhow::Result<()>
    {
        let records = [TestRecord::new("test_a", "test-group")];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(10));

        let scheduler = Scheduler::new(1, &tests, &durations, &HashMap::new(), None);

        let batch = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for batch"))?
            .ok_or_else(|| anyhow::anyhow!("expected a batch"))?;

        // Future completes instantly, well within 2 * 10s threshold
        let result = scheduler.register_running_batch(&batch, async { 42 }).await;
        assert_eq!(result, 42);

        // Queue should be empty — no re-queuing occurred
        assert!(
            tokio::time::timeout(Duration::from_millis(10), scheduler.pop())
                .await
                .is_err(),
            "queue should be empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_register_running_batch_requeues_and_splits_multi_test_batch() -> anyhow::Result<()>
    {
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_b", "test-group"),
            TestRecord::new("test_c", "test-group"),
            TestRecord::new("test_d", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        for r in &records {
            durations.insert(r.id.clone(), Duration::from_millis(1));
        }

        // 1 worker => all 4 tests in one batch, estimated_load = 4ms
        // requeue_after = 2 * 4ms = 8ms — sleep of 50ms will exceed that
        let scheduler = Scheduler::new(1, &tests, &durations, &HashMap::new(), None);

        let batch = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for batch"))?
            .ok_or_else(|| anyhow::anyhow!("expected a batch"))?;
        assert_eq!(batch.tests.len(), 4);

        scheduler
            .register_running_batch(&batch, tokio::time::sleep(Duration::from_millis(50)))
            .await;

        // Should have 2 halves re-queued
        let first = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for first half"))?
            .ok_or_else(|| anyhow::anyhow!("expected first half"))?;
        let second = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for second half"))?
            .ok_or_else(|| anyhow::anyhow!("expected second half"))?;

        assert_eq!(first.tests.len(), 2);
        assert_eq!(second.tests.len(), 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), scheduler.pop())
                .await
                .is_err(),
            "queue should be empty after two pops"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_register_running_batch_requeues_single_test_batch() -> anyhow::Result<()> {
        let records = [TestRecord::new("test_a", "test-group")];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_millis(1));

        // 1 worker, 1 test, estimated_load = 1ms, requeue_after = 2ms
        let scheduler = Scheduler::new(1, &tests, &durations, &HashMap::new(), None);

        let batch = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for batch"))?
            .ok_or_else(|| anyhow::anyhow!("expected a batch"))?;
        assert_eq!(batch.tests.len(), 1);

        scheduler
            .register_running_batch(&batch, tokio::time::sleep(Duration::from_millis(50)))
            .await;

        // Single-test batch is cloned and re-queued as-is
        let requeued = tokio::time::timeout(Duration::from_millis(100), scheduler.pop())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for requeued batch"))?
            .ok_or_else(|| anyhow::anyhow!("expected requeued batch"))?;
        assert_eq!(requeued.tests.len(), 1);
        assert_eq!(requeued.tests[0].id(), "test_a");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), scheduler.pop())
                .await
                .is_err(),
            "queue should be empty after one pop"
        );
        Ok(())
    }
}
