//! Test scheduling and distribution across parallel sandboxes.

use std::collections::HashMap;
use std::time::Duration;

use rand::seq::SliceRandom;
use rand::thread_rng;

use crate::framework::TestInstance;

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
    /// # Arguments
    ///
    /// * `tests` - Tests to schedule
    /// * `durations` - Historical test durations from previous runs.
    ///   Tests not in the map use `default_duration`.
    /// * `default_duration` - Duration to use for tests without historical data.
    ///
    /// # Algorithm
    ///
    /// 1. Sort tests by duration (descending)
    /// 2. For each test, assign to the worker with smallest current load
    /// 3. Sort batches by total duration (descending) so heaviest starts first
    ///
    /// This is a greedy 4/3-approximation for the multiprocessor scheduling problem.
    pub fn schedule_lpt<'a>(
        &self,
        tests: &[TestInstance<'a>],
        durations: &HashMap<String, Duration>,
        default_duration: Duration,
    ) -> Vec<Vec<TestInstance<'a>>> {
        if tests.is_empty() {
            return Vec::new();
        }

        // Count instances per test ID to determine max retry count
        let mut instance_counts: HashMap<&str, usize> = HashMap::new();
        for test in tests {
            *instance_counts.entry(test.id()).or_insert(0) += 1;
        }
        let max_instances = instance_counts.values().copied().max().unwrap_or(1);

        // Assert we have enough workers to avoid putting the same test in the same batch
        assert!(
            self.max_parallel >= max_instances,
            "Not enough workers ({}) for retry count ({}). Each test instance must run in a separate sandbox.",
            self.max_parallel,
            max_instances
        );

        // Look up durations for each test
        let mut tests_with_duration: Vec<(TestInstance<'a>, Duration)> = tests
            .iter()
            .map(|t| {
                let duration = durations.get(t.id()).copied().unwrap_or(default_duration);
                (*t, duration)
            })
            .collect();

        // Sort by duration descending (longest first)
        tests_with_duration.sort_by(|a, b| b.1.cmp(&a.1));

        // Initialize batches
        let num_batches = self.max_parallel.min(tests.len());
        let mut batches: Vec<Vec<TestInstance<'a>>> =
            (0..num_batches).map(|_| Vec::new()).collect();
        let mut batch_loads: Vec<Duration> = vec![Duration::ZERO; num_batches];

        // Track which test IDs are in each batch to prevent duplicates
        let mut batch_test_ids: Vec<std::collections::HashSet<String>> = (0..num_batches)
            .map(|_| std::collections::HashSet::new())
            .collect();

        // LPT assignment: assign each test to the batch with minimum load
        for (test, duration) in tests_with_duration {
            let test_id = test.id();

            // Find the batch with minimum load that doesn't already have this test
            let target_idx = batch_loads
                .iter()
                .enumerate()
                .filter(|(idx, _)| !batch_test_ids[*idx].contains(test_id))
                .min_by_key(|(_, load)| *load)
                .map(|(idx, _)| idx)
                // Safe: assertion above ensures enough batches for all test instances
                .unwrap_or(0);

            batches[target_idx].push(test);
            batch_loads[target_idx] += duration;
            batch_test_ids[target_idx].insert(test_id.to_string());
        }

        // Sort batches by load (heaviest first) for optimal Modal scheduling
        let mut batches_with_loads: Vec<_> = batches.into_iter().zip(batch_loads).collect();
        batches_with_loads.sort_by(|a, b| b.1.cmp(&a.1));

        // Extract just the batches, removing empty ones
        batches_with_loads
            .into_iter()
            .map(|(batch, _)| batch)
            .filter(|b| !b.is_empty())
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
            scheduler.schedule_lpt(&[], &durations, Duration::from_secs(1));
        assert!(batches.is_empty());
    }

    #[test]
    fn test_schedule_lpt_balances_load() {
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("slow_test"),
            TestRecord::new("medium_test"),
            TestRecord::new("fast_test"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("slow_test".to_string(), Duration::from_secs(10));
        durations.insert("medium_test".to_string(), Duration::from_secs(5));
        durations.insert("fast_test".to_string(), Duration::from_secs(1));

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));

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
            TestRecord::new("test_a"),
            TestRecord::new("test_b"),
            TestRecord::new("test_c"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(1));
        durations.insert("test_b".to_string(), Duration::from_secs(5));
        durations.insert("test_c".to_string(), Duration::from_secs(3));

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));

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
            TestRecord::new("known_slow"),
            TestRecord::new("unknown_test"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("known_slow".to_string(), Duration::from_secs(10));
        // unknown_test will use default of 1 second

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));

        assert_eq!(batches.len(), 2);
        // known_slow (10s) should be in heaviest batch
        assert_eq!(batches[0][0].id(), "known_slow");
        assert_eq!(batches[1][0].id(), "unknown_test");
    }

    #[test]
    fn test_schedule_single() {
        let scheduler = Scheduler::new(4);
        let records = [TestRecord::new("test1")];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
        let batches = scheduler.schedule(&tests);

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_schedule_round_robin() {
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("test1"),
            TestRecord::new("test2"),
            TestRecord::new("test3"),
            TestRecord::new("test4"),
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
            TestRecord::new("test1"),
            TestRecord::new("test2"),
            TestRecord::new("test3"),
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
            .map(|i| TestRecord::new(format!("test{}", i)))
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
            TestRecord::new("test_a"),
            TestRecord::new("test_a"), // retry 1
            TestRecord::new("test_a"), // retry 2
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(5));

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));

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
            TestRecord::new("test_a"),
            TestRecord::new("test_a"), // retry
            TestRecord::new("test_b"),
            TestRecord::new("test_c"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let mut durations = HashMap::new();
        durations.insert("test_a".to_string(), Duration::from_secs(10));
        durations.insert("test_b".to_string(), Duration::from_secs(5));
        durations.insert("test_c".to_string(), Duration::from_secs(1));

        let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));

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
    #[should_panic(expected = "Not enough workers")]
    fn test_schedule_lpt_panics_insufficient_workers_for_retries() {
        // 2 workers but 3 instances of same test - should panic
        let scheduler = Scheduler::new(2);
        let records = [
            TestRecord::new("test_a"),
            TestRecord::new("test_a"),
            TestRecord::new("test_a"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let durations = HashMap::new();
        scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));
    }
}
