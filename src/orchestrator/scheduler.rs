//! Test scheduling and distribution across parallel sandboxes.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rand::seq::SliceRandom;
use rand::thread_rng;

use crate::framework::TestInstance;

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
struct Batch<'a> {
    tests: Vec<TestInstance<'a>>,
    load: Duration,
    command_len: usize,
    test_ids: HashSet<String>,
}

impl<'a> Batch<'a> {
    fn new() -> Self {
        Self {
            tests: Vec::new(),
            load: Duration::ZERO,
            command_len: 0,
            test_ids: HashSet::new(),
        }
    }

    fn add(&mut self, test: TestInstance<'a>, duration: Duration) {
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
/// The scheduler is responsible for creating batches of tests that can
/// be executed in parallel across multiple sandboxes. It doesn't know
/// about the actual sandboxes - it just creates batches based on the
/// configured parallelism level.
pub struct Scheduler {
    max_parallel: usize,
}

impl Scheduler {
    /// Creates a new scheduler with the given parallelism limit.
    ///
    /// # Arguments
    ///
    /// * `max_parallel` - Maximum number of parallel batches/sandboxes.
    ///   Minimum is 1 (values below 1 are clamped).
    pub fn new(max_parallel: usize) -> Self {
        Self {
            max_parallel: max_parallel.max(1),
        }
    }

    /// Schedules tests into batches using round-robin distribution.
    ///
    /// Tests are distributed evenly across up to `max_parallel` batches.
    /// This is the default scheduling strategy that balances load across
    /// sandboxes.
    ///
    /// # Returns
    ///
    /// A vector of batches. Each batch is a vector of tests that will
    /// run sequentially in the same sandbox. Empty batches are removed.
    pub fn schedule<'a>(&self, tests: &[TestInstance<'a>]) -> Vec<Vec<TestInstance<'a>>> {
        if tests.is_empty() {
            return Vec::new();
        }

        // Simple round-robin distribution
        let mut batches: Vec<Vec<TestInstance<'a>>> =
            (0..self.max_parallel).map(|_| Vec::new()).collect();

        for (i, test) in tests.iter().enumerate() {
            let batch_idx = i % self.max_parallel;
            batches[batch_idx].push(*test);
        }

        // Remove empty batches
        batches.retain(|b| !b.is_empty());

        batches
    }

    /// Schedules tests with random distribution across sandboxes.
    ///
    /// Shuffles tests randomly before distributing them across sandboxes
    /// using round-robin. This helps avoid systematic biases in test ordering.
    ///
    /// # Returns
    ///
    /// A vector of batches with randomly distributed tests.
    pub fn schedule_random<'a>(&self, tests: &[TestInstance<'a>]) -> Vec<Vec<TestInstance<'a>>> {
        if tests.is_empty() {
            return Vec::new();
        }

        // Shuffle tests randomly
        let mut shuffled: Vec<TestInstance<'a>> = tests.to_vec();
        shuffled.shuffle(&mut thread_rng());

        // Round-robin distribution of shuffled tests
        let mut batches: Vec<Vec<TestInstance<'a>>> =
            (0..self.max_parallel).map(|_| Vec::new()).collect();

        for (i, test) in shuffled.into_iter().enumerate() {
            let batch_idx = i % self.max_parallel;
            batches[batch_idx].push(test);
        }

        // Remove empty batches
        batches.retain(|b| !b.is_empty());

        batches
    }

    /// Schedules tests using Longest Processing Time First (LPT) algorithm.
    ///
    /// Uses historical test durations to minimize total execution time (makespan).
    /// Tests are sorted by duration descending and assigned to the worker with
    /// the smallest current total workload.
    ///
    /// The returned batches are sorted by total duration descending, so the
    /// heaviest batch is first. This ensures it gets scheduled first with Modal.
    ///
    /// When `max_batch_duration` is set, batches that would exceed the cap are
    /// not eligible for assignment, and new batches are created as needed. This
    /// means the total number of batches may exceed `max_parallel`.
    ///
    /// # Arguments
    ///
    /// * `tests` - Tests to schedule
    /// * `durations` - Historical test durations from previous runs.
    ///   Tests not in the map use `default_duration`.
    /// * `default_duration` - Duration to use for tests without historical data.
    /// * `max_batch_duration` - Optional cap on the total duration of each batch.
    ///   A single test that exceeds the cap is still placed alone in its own batch.
    ///
    /// # Algorithm
    ///
    /// 1. Sort tests by duration (descending)
    /// 2. For each test, assign to the worker with smallest current load
    ///    that does not already contain the same test ID, and (if capped)
    ///    whose load plus the test duration would not exceed the cap
    ///    (or is empty)
    /// 3. If no eligible batch exists, create a new batch
    /// 4. Sort batches by total duration (descending) so heaviest starts first
    ///
    /// This is a greedy 4/3-approximation for the multiprocessor scheduling problem.
    pub fn schedule_lpt<'a>(
        &self,
        tests: &[TestInstance<'a>],
        durations: &HashMap<String, Duration>,
        default_duration: Duration,
        max_batch_duration: Option<Duration>,
    ) -> Vec<Vec<TestInstance<'a>>> {
        if tests.is_empty() {
            return Vec::new();
        }

        // Look up durations for each test, sorted longest-first
        let mut tests_with_duration: Vec<_> = tests
            .iter()
            .map(|t| {
                (
                    *t,
                    durations.get(t.id()).copied().unwrap_or(default_duration),
                )
            })
            .collect();
        tests_with_duration.sort_by(|a, b| b.1.cmp(&a.1));

        // Initialize batches
        let num_batches = self.max_parallel.min(tests.len());
        let mut batches: Vec<Batch<'a>> = (0..num_batches).map(|_| Batch::new()).collect();

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

        batches
            .into_iter()
            .filter(|b| !b.is_empty())
            .map(|b| b.tests)
            .collect()
    }

    /// Schedules tests with a maximum batch size.
    ///
    /// Creates batches of at most `max_batch_size` tests. This may create
    /// more batches than `max_parallel`, but each batch is limited in size.
    /// Useful when sandboxes have resource constraints.
    ///
    /// # Arguments
    ///
    /// * `tests` - Tests to schedule
    /// * `max_batch_size` - Maximum tests per batch
    pub fn schedule_with_batch_size<'a>(
        &self,
        tests: &[TestInstance<'a>],
        max_batch_size: usize,
    ) -> Vec<Vec<TestInstance<'a>>> {
        if tests.is_empty() {
            return Vec::new();
        }

        let mut batches = Vec::new();

        for chunk in tests.chunks(max_batch_size) {
            batches.push(chunk.to_vec());
        }

        batches
    }

    /// Schedules each test in its own batch for maximum isolation.
    ///
    /// Creates one batch per test, ensuring each test runs in a fresh
    /// sandbox. Useful for integration tests that require complete
    /// isolation or modify shared state.
    ///
    /// **Note**: This ignores `max_parallel` for batch creation but the
    /// orchestrator still limits concurrent execution.
    pub fn schedule_individual<'a>(
        &self,
        tests: &[TestInstance<'a>],
    ) -> Vec<Vec<TestInstance<'a>>> {
        tests.iter().map(|t| vec![*t]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::TestRecord;

    const MAX_BATCH_DURATION: Duration = Duration::from_secs(10);

    #[test]
    fn test_schedule_empty() {
        let scheduler = Scheduler::new(4);
        let batches: Vec<Vec<TestInstance>> = scheduler.schedule(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_schedule_lpt_empty() {
        let scheduler = Scheduler::new(4);
        let durations = HashMap::new();
        let batches: Vec<Vec<TestInstance>> =
            scheduler.schedule_lpt(&[], &durations, Duration::from_secs(1), None);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_schedule_lpt_balances_load() {
        let scheduler = Scheduler::new(2);
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

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1), None);

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

    #[test]
    fn test_schedule_lpt_heaviest_batch_first() {
        let scheduler = Scheduler::new(3);
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

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1), None);

        // Each test in its own batch (3 workers, 3 tests)
        // Sorted by duration: test_b (5s), test_c (3s), test_a (1s)
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0][0].id(), "test_b"); // Heaviest first
        assert_eq!(batches[1][0].id(), "test_c");
        assert_eq!(batches[2][0].id(), "test_a");
    }

    #[test]
    fn test_schedule_lpt_uses_default_for_unknown() {
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("known_slow", "test-group"),
            TestRecord::new("unknown_test", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("known_slow".to_string(), Duration::from_secs(10));
        // unknown_test will use default of 1 second

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1), None);

        assert_eq!(batches.len(), 2);
        // known_slow (10s) should be in heaviest batch
        assert_eq!(batches[0][0].id(), "known_slow");
        assert_eq!(batches[1][0].id(), "unknown_test");
    }

    #[test]
    fn test_schedule_single() {
        let scheduler = Scheduler::new(4);
        let records = [TestRecord::new("test1", "test-group")];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let batches = scheduler.schedule(&tests);

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_schedule_round_robin() {
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("test1", "test-group"),
            TestRecord::new("test2", "test-group"),
            TestRecord::new("test3", "test-group"),
            TestRecord::new("test4", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let batches = scheduler.schedule(&tests);

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 2);
        assert_eq!(batches[0][0].id(), "test1");
        assert_eq!(batches[0][1].id(), "test3");
        assert_eq!(batches[1][0].id(), "test2");
        assert_eq!(batches[1][1].id(), "test4");
    }

    #[test]
    fn test_schedule_individual() {
        let scheduler = Scheduler::new(4);
        let records = [
            TestRecord::new("test1", "test-group"),
            TestRecord::new("test2", "test-group"),
            TestRecord::new("test3", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let batches = scheduler.schedule_individual(&tests);

        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
        assert_eq!(batches[2].len(), 1);
    }

    #[test]
    fn test_schedule_with_batch_size() {
        let scheduler = Scheduler::new(4);
        let records: Vec<_> = (0..10)
            .map(|i| TestRecord::new(format!("test{}", i), "test-group"))
            .collect();
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let batches = scheduler.schedule_with_batch_size(&tests, 3);

        assert_eq!(batches.len(), 4);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 3);
        assert_eq!(batches[2].len(), 3);
        assert_eq!(batches[3].len(), 1);
    }

    #[test]
    fn test_schedule_lpt_duplicate_prevention() {
        // Simulate retry scenario: same test appears multiple times
        let scheduler = Scheduler::new(3);
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"), // retry 1
            TestRecord::new("test_a", "test-group"), // retry 2
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(5));

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1), None);

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

    #[test]
    fn test_schedule_lpt_mixed_duplicates_and_unique() {
        // Mix of retried and unique tests
        let scheduler = Scheduler::new(3);
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

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1), None);

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

    #[test]
    fn test_schedule_lpt_creates_extra_batches_for_retries() {
        // 2 workers but 3 instances of same test — creates 3 batches (one per instance)
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"),
            TestRecord::new("test_a", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let durations = HashMap::new();
        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1), None);

        // Each instance must be in a separate batch
        assert_eq!(batches.len(), 3);
        for batch in &batches {
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].id(), "test_a");
        }
    }

    #[test]
    fn test_schedule_lpt_respects_max_batch_duration() {
        let scheduler = Scheduler::new(2);
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
        let batches = scheduler.schedule_lpt(
            &tests,
            &durations,
            Duration::from_secs(1),
            Some(MAX_BATCH_DURATION),
        );

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

    #[test]
    fn test_schedule_lpt_long_test_gets_own_batch() -> anyhow::Result<()> {
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("slow_test", "test-group"),
            TestRecord::new("fast_test", "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("slow_test".to_string(), Duration::from_secs(15));
        durations.insert("fast_test".to_string(), Duration::from_secs(2));

        // slow_test exceeds the cap on its own, but that's fine — single test in batch
        let batches = scheduler.schedule_lpt(
            &tests,
            &durations,
            Duration::from_secs(1),
            Some(MAX_BATCH_DURATION),
        );

        assert_eq!(batches.len(), 2);
        // slow_test should be alone in its batch
        let slow_batch = batches
            .iter()
            .find(|b| b.iter().any(|t| t.id() == "slow_test"))
            .ok_or_else(|| anyhow::anyhow!("slow_test batch not found"))?;
        assert_eq!(slow_batch.len(), 1);
        Ok(())
    }

    #[test]
    fn test_schedule_lpt_creates_extra_batches_for_duration_cap() {
        // 5 tests of 3s each, max_parallel=2, cap=10s
        // Can fit 3 tests per batch (9s < 10s), so need at least 2 batches
        // But only 2 workers, so tests get split: batch 0 = 3 tests (9s), batch 1 = 2 tests (6s)
        let scheduler = Scheduler::new(2);
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
        let batches = scheduler.schedule_lpt(
            &tests,
            &durations,
            Duration::from_secs(1),
            Some(MAX_BATCH_DURATION),
        );

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

    #[test]
    fn test_schedule_lpt_splits_on_command_length() {
        let scheduler = Scheduler::new(1);
        // Create tests whose IDs together exceed MAX_BATCH_COMMAND_LEN
        let long_name = "a".repeat(MAX_BATCH_COMMAND_LEN / 2 + 1);
        let records = [
            TestRecord::new(format!("{long_name}_1"), "test-group"),
            TestRecord::new(format!("{long_name}_2"), "test-group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let batches = scheduler.schedule_lpt(&tests, &HashMap::new(), Duration::from_secs(1), None);

        // Two tests that each use >half the command length budget must be in separate batches
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn test_schedule_lpt_groups_short_commands() {
        let scheduler = Scheduler::new(1);
        // Create many tests with short IDs that fit in one batch
        let records: Vec<_> = (0..100)
            .map(|i| TestRecord::new(format!("t{i}"), "test-group"))
            .collect();
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let batches = scheduler.schedule_lpt(&tests, &HashMap::new(), Duration::from_secs(0), None);

        // Total command length is ~400 chars, well under 30k — should be 1 batch
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 100);
    }
}
