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

use std::collections::HashMap;
use std::time::Duration;

use rand::seq::SliceRandom;
use rand::thread_rng;

use crate::framework::TestInstance;

/// Look up a test's duration with suffix matching.
///
/// First tries an exact match. If not found, looks for a key where the
/// test_id ends with that key (handling path prefix mismatches like
/// `libs/mng/imbue/...` vs `imbue/...`).
fn lookup_duration_with_suffix_match(
    durations: &HashMap<String, Duration>,
    test_id: &str,
) -> Option<Duration> {
    // Try exact match first
    if let Some(&duration) = durations.get(test_id) {
        return Some(duration);
    }

    // Try suffix matching: find a key where test_id ends with "/" + key
    for (key, &duration) in durations {
        if test_id.ends_with(&format!("/{}", key)) {
            return Some(duration);
        }
    }

    None
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
    ///
    /// # Example
    ///
    /// ```
    /// use offload::orchestrator::Scheduler;
    /// use offload::framework::TestRecord;
    /// use std::collections::HashMap;
    /// use std::time::Duration;
    ///
    /// let scheduler = Scheduler::new(2);
    /// let records = vec![
    ///     TestRecord::new("slow_test"),
    ///     TestRecord::new("fast_test"),
    ///     TestRecord::new("medium_test"),
    /// ];
    /// let tests: Vec<_> = records.iter().map(|r| r.test()).collect();
    ///
    /// let mut durations = HashMap::new();
    /// durations.insert("slow_test".to_string(), Duration::from_secs(10));
    /// durations.insert("fast_test".to_string(), Duration::from_secs(1));
    /// durations.insert("medium_test".to_string(), Duration::from_secs(5));
    ///
    /// let batches = scheduler.schedule_lpt(&tests, &durations, Duration::from_secs(1));
    /// // Batch 0 (heaviest): slow_test (10s)
    /// // Batch 1: medium_test (5s), fast_test (1s) = 6s total
    /// assert_eq!(batches.len(), 2);
    /// ```
    pub fn schedule_lpt<'a>(
        &self,
        tests: &[TestInstance<'a>],
        durations: &HashMap<String, Duration>,
        default_duration: Duration,
    ) -> Vec<Vec<TestInstance<'a>>> {
        println!("\n============================================================");
        println!("LPT SCHEDULER START");
        println!("============================================================");
        println!(
            "Input: {} tests, {} workers, {} known durations, default={:?}",
            tests.len(),
            self.max_parallel,
            durations.len(),
            default_duration
        );

        if tests.is_empty() {
            println!("No tests to schedule, returning empty");
            return Vec::new();
        }

        // Count instances per test ID to determine max retry count
        let mut instance_counts: HashMap<&str, usize> = HashMap::new();
        for test in tests {
            *instance_counts.entry(test.id()).or_insert(0) += 1;
        }
        let max_instances = instance_counts.values().copied().max().unwrap_or(1);
        let unique_tests = instance_counts.len();

        println!(
            "Unique tests: {}, max instances per test: {}",
            unique_tests, max_instances
        );

        // Assert we have enough workers to avoid putting the same test in the same batch
        assert!(
            self.max_parallel >= max_instances,
            "Not enough workers ({}) for retry count ({}). Each test instance must run in a separate sandbox.",
            self.max_parallel,
            max_instances
        );

        // Phase 1: Look up durations for each test (with suffix matching for path prefix mismatches)
        println!("\n--- PHASE 1: Duration Lookup ---");
        let mut known_count = 0;
        let mut default_count = 0;
        let mut tests_with_duration: Vec<(TestInstance<'a>, Duration)> = tests
            .iter()
            .map(|t| {
                let (duration, source) =
                    if let Some(d) = lookup_duration_with_suffix_match(durations, t.id()) {
                        known_count += 1;
                        (d, "junit.xml")
                    } else {
                        default_count += 1;
                        (default_duration, "DEFAULT")
                    };
                println!("  {:?} <- {} [{}]", duration, t.id(), source);
                (*t, duration)
            })
            .collect();

        println!(
            "Duration lookup complete: {} from junit.xml, {} using default",
            known_count, default_count
        );

        // Phase 2: Sort by duration descending
        println!("\n--- PHASE 2: Sort by Duration (descending) ---");
        tests_with_duration.sort_by(|a, b| b.1.cmp(&a.1));
        println!("Sorted order (longest first):");
        for (i, (test, duration)) in tests_with_duration.iter().enumerate() {
            println!("  {}: {:?} - {}", i + 1, duration, test.id());
        }

        // Phase 3: Initialize batches
        let num_batches = self.max_parallel.min(tests.len());
        println!("\n--- PHASE 3: Initialize {} Batches ---", num_batches);
        let mut batches: Vec<Vec<TestInstance<'a>>> =
            (0..num_batches).map(|_| Vec::new()).collect();
        let mut batch_loads: Vec<Duration> = vec![Duration::ZERO; num_batches];

        // Phase 4: LPT assignment (with duplicate prevention)
        println!("\n--- PHASE 4: LPT Assignment (with duplicate prevention) ---");

        // Track which test IDs are in each batch to prevent duplicates
        let mut batch_test_ids: Vec<std::collections::HashSet<String>> =
            (0..num_batches).map(|_| std::collections::HashSet::new()).collect();

        for (test, duration) in tests_with_duration {
            let test_id = test.id();

            // Find the batch with minimum load that doesn't already have this test
            let mut candidates: Vec<(usize, Duration)> = batch_loads
                .iter()
                .enumerate()
                .filter(|(idx, _)| !batch_test_ids[*idx].contains(test_id))
                .map(|(idx, load)| (idx, *load))
                .collect();

            // Sort by load (ascending)
            candidates.sort_by_key(|(_, load)| *load);

            let target_idx = candidates.first().map(|(idx, _)| *idx).expect(
                "No available batch for test - this should be impossible due to earlier assertion",
            );

            let old_load = batch_loads[target_idx];
            batches[target_idx].push(test);
            batch_loads[target_idx] += duration;
            batch_test_ids[target_idx].insert(test_id.to_string());

            println!(
                "  Assign {} ({:?}) -> Batch {} (load: {:?} -> {:?})",
                test_id,
                duration,
                target_idx,
                old_load,
                batch_loads[target_idx]
            );

            // Show current batch loads
            let loads_str: Vec<String> = batch_loads
                .iter()
                .enumerate()
                .map(|(i, l)| format!("B{}={:?}", i, l))
                .collect();
            println!("    Current loads: [{}]", loads_str.join(", "));
        }

        // Phase 5: Sort batches by load (heaviest first)
        println!("\n--- PHASE 5: Sort Batches (heaviest first) ---");
        println!("Before sort:");
        for (i, load) in batch_loads.iter().enumerate() {
            println!("  Batch {}: {:?} ({} tests)", i, load, batches[i].len());
        }

        let mut batches_with_loads: Vec<_> = batches.into_iter().zip(batch_loads).collect();
        batches_with_loads.sort_by(|a, b| b.1.cmp(&a.1));

        println!("After sort (heaviest first for Modal):");
        for (i, (batch, load)) in batches_with_loads.iter().enumerate() {
            println!("  Batch {}: {:?} ({} tests)", i, load, batch.len());
            for test in batch {
                println!("    - {}", test.id());
            }
        }

        // Phase 6: Final summary
        println!("\n--- PHASE 6: Final Summary ---");
        let total_duration: Duration = batches_with_loads.iter().map(|(_, l)| *l).sum();
        let max_duration = batches_with_loads
            .first()
            .map(|(_, l)| *l)
            .unwrap_or(Duration::ZERO);
        println!("Total work: {:?}", total_duration);
        println!("Makespan (longest batch): {:?}", max_duration);
        println!(
            "Parallelism efficiency: {:.1}%",
            if max_duration.as_secs_f64() > 0.0 {
                (total_duration.as_secs_f64() / (max_duration.as_secs_f64() * num_batches as f64))
                    * 100.0
            } else {
                100.0
            }
        );
        println!("============================================================");
        println!("LPT SCHEDULER END");
        println!("============================================================\n");

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
}
