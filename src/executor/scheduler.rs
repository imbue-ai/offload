//! Test scheduling and distribution.
//!
//! This module handles distributing tests across available sandboxes
//! for parallel execution.

use crate::discovery::TestCase;

/// Scheduler distributes tests across parallel sandboxes.
pub struct Scheduler {
    max_parallel: usize,
}

impl Scheduler {
    /// Create a new scheduler with the given parallelism limit.
    pub fn new(max_parallel: usize) -> Self {
        Self {
            max_parallel: max_parallel.max(1),
        }
    }

    /// Schedule tests into batches for parallel execution.
    ///
    /// Returns a vector of batches, where each batch is a vector of tests
    /// that will run in the same sandbox.
    pub fn schedule(&self, tests: &[TestCase]) -> Vec<Vec<TestCase>> {
        if tests.is_empty() {
            return Vec::new();
        }

        // Simple round-robin distribution
        let mut batches: Vec<Vec<TestCase>> = (0..self.max_parallel)
            .map(|_| Vec::new())
            .collect();

        for (i, test) in tests.iter().enumerate() {
            let batch_idx = i % self.max_parallel;
            batches[batch_idx].push(test.clone());
        }

        // Remove empty batches
        batches.retain(|b| !b.is_empty());

        batches
    }

    /// Schedule tests with a maximum batch size.
    ///
    /// This creates more batches but limits how many tests run per sandbox.
    pub fn schedule_with_batch_size(&self, tests: &[TestCase], max_batch_size: usize) -> Vec<Vec<TestCase>> {
        if tests.is_empty() {
            return Vec::new();
        }

        let mut batches = Vec::new();

        for chunk in tests.chunks(max_batch_size) {
            batches.push(chunk.to_vec());
        }

        batches
    }

    /// Schedule tests for individual execution (one test per sandbox).
    ///
    /// This is useful for integration tests that need complete isolation.
    pub fn schedule_individual(&self, tests: &[TestCase]) -> Vec<Vec<TestCase>> {
        tests.iter().map(|t| vec![t.clone()]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test(id: &str) -> TestCase {
        TestCase::new(id)
    }

    #[test]
    fn test_schedule_empty() {
        let scheduler = Scheduler::new(4);
        let batches = scheduler.schedule(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_schedule_single() {
        let scheduler = Scheduler::new(4);
        let tests = vec![make_test("test1")];
        let batches = scheduler.schedule(&tests);

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_schedule_round_robin() {
        let scheduler = Scheduler::new(2);
        let tests = vec![
            make_test("test1"),
            make_test("test2"),
            make_test("test3"),
            make_test("test4"),
        ];
        let batches = scheduler.schedule(&tests);

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[1].len(), 2);
        assert_eq!(batches[0][0].id, "test1");
        assert_eq!(batches[0][1].id, "test3");
        assert_eq!(batches[1][0].id, "test2");
        assert_eq!(batches[1][1].id, "test4");
    }

    #[test]
    fn test_schedule_individual() {
        let scheduler = Scheduler::new(4);
        let tests = vec![
            make_test("test1"),
            make_test("test2"),
            make_test("test3"),
        ];
        let batches = scheduler.schedule_individual(&tests);

        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
        assert_eq!(batches[2].len(), 1);
    }

    #[test]
    fn test_schedule_with_batch_size() {
        let scheduler = Scheduler::new(4);
        let tests: Vec<_> = (0..10).map(|i| make_test(&format!("test{}", i))).collect();
        let batches = scheduler.schedule_with_batch_size(&tests, 3);

        assert_eq!(batches.len(), 4);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 3);
        assert_eq!(batches[2].len(), 3);
        assert_eq!(batches[3].len(), 1);
    }
}
