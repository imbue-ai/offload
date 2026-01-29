//! Test scheduling and distribution.
//!
//! This module handles distributing tests across available sandboxes
//! for parallel execution. The scheduler creates batches of tests that
//! can be executed independently.
//!
//! # Scheduling Strategies
//!
//! The scheduler provides multiple strategies for test distribution:
//!
//! | Method | Description | Use Case |
//! |--------|-------------|----------|
//! | [`schedule`](Scheduler::schedule) | Round-robin across sandboxes | Default, balanced load |
//! | [`schedule_with_batch_size`](Scheduler::schedule_with_batch_size) | Fixed batch sizes | Limited per-sandbox resources |
//! | [`schedule_individual`](Scheduler::schedule_individual) | One test per sandbox | Maximum isolation |
//!
//! # Example
//!
//! ```
//! use offload::orchestrator::Scheduler;
//! use offload::framework::TestRecord;
//!
//! let scheduler = Scheduler::new(4); // 4 parallel sandboxes
//!
//! let records: Vec<TestRecord> = (0..10)
//!     .map(|i| TestRecord::new(format!("test_{}", i)))
//!     .collect();
//!
//! let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
//! let batches = scheduler.schedule(&tests);
//! assert_eq!(batches.len(), 4); // 4 batches for 4 sandboxes
//! ```

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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::Scheduler;
    ///
    /// let scheduler = Scheduler::new(4);
    /// ```
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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::Scheduler;
    /// use offload::framework::TestRecord;
    ///
    /// let scheduler = Scheduler::new(2);
    /// let records = vec![
    ///     TestRecord::new("test_a"),
    ///     TestRecord::new("test_b"),
    ///     TestRecord::new("test_c"),
    ///     TestRecord::new("test_d"),
    /// ];
    /// let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
    ///
    /// let batches = scheduler.schedule(&tests);
    /// // Batch 0: test_a, test_c
    /// // Batch 1: test_b, test_d
    /// assert_eq!(batches.len(), 2);
    /// assert_eq!(batches[0].len(), 2);
    /// ```
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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::Scheduler;
    /// use offload::framework::TestRecord;
    ///
    /// let scheduler = Scheduler::new(2);
    /// let records = vec![
    ///     TestRecord::new("test_a"),
    ///     TestRecord::new("test_b"),
    ///     TestRecord::new("test_c"),
    ///     TestRecord::new("test_d"),
    /// ];
    /// let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
    ///
    /// let batches = scheduler.schedule_random(&tests);
    /// assert_eq!(batches.len(), 2);
    /// // Tests are randomly distributed
    /// ```
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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::Scheduler;
    /// use offload::framework::TestRecord;
    ///
    /// let scheduler = Scheduler::new(10);
    /// let records: Vec<_> = (0..25).map(|i| TestRecord::new(format!("test_{}", i))).collect();
    /// let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
    ///
    /// let batches = scheduler.schedule_with_batch_size(&tests, 10);
    /// assert_eq!(batches.len(), 3);
    /// assert_eq!(batches[0].len(), 10);
    /// assert_eq!(batches[1].len(), 10);
    /// assert_eq!(batches[2].len(), 5);
    /// ```
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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::Scheduler;
    /// use offload::framework::TestRecord;
    ///
    /// let scheduler = Scheduler::new(2);
    /// let records = vec![
    ///     TestRecord::new("test_a"),
    ///     TestRecord::new("test_b"),
    ///     TestRecord::new("test_c"),
    /// ];
    /// let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
    ///
    /// let batches = scheduler.schedule_individual(&tests);
    /// assert_eq!(batches.len(), 3);
    /// assert!(batches.iter().all(|b| b.len() == 1));
    /// ```
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
}
