//! JSONL-backed test history storage.
//!
//! Implements `TestHistoryStore` using a local JSONL file that can be checked
//! into source control. Maintains bounded storage via weighted reservoir sampling.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use super::jsonl::{CompactSample, HistoryRecord, TestValues, parse_line, serialize_record};
use super::reservoir::{Sample, WeightedReservoir};
use super::{
    DurationStats, HistoryError, OutcomeStats, TestAttemptResult, TestHistoryStore, TestStatistics,
};

/// Local file-backed implementation of `TestHistoryStore`.
///
/// Stores test history in a JSONL file with one record per test. Each record
/// maintains weighted reservoirs for success and failure samples, enabling
/// percentile estimation with bounded storage.
pub struct JsonlHistoryStore {
    records: HashMap<(String, String), HistoryRecord>,
    path: PathBuf,
    reservoir_size: usize,
    default_duration_secs: f64,
}

impl JsonlHistoryStore {
    /// Creates a new empty store that will save to the given path.
    pub fn new(path: PathBuf, reservoir_size: usize, default_duration_secs: f64) -> Self {
        Self {
            records: HashMap::new(),
            path,
            reservoir_size,
            default_duration_secs,
        }
    }

    /// Get scheduling durations for all tests in a config.
    ///
    /// Returns a HashMap mapping test_id -> expected_duration, suitable for
    /// use with the LPT scheduler. Uses `expected_duration()` for each test,
    /// which applies the weighted P75 fallback chain.
    pub fn get_scheduling_durations(&self, config: &str) -> HashMap<String, Duration> {
        self.records
            .iter()
            .filter(|((c, _), _)| c == config)
            .map(|((_, test_id), _)| (test_id.clone(), self.expected_duration(config, test_id)))
            .collect()
    }

    /// Loads an existing store from disk, or creates an empty one if the file does not exist.
    pub fn load(
        path: &std::path::Path,
        reservoir_size: usize,
        default_duration_secs: f64,
    ) -> Result<Self, HistoryError> {
        let mut store = Self::new(path.to_path_buf(), reservoir_size, default_duration_secs);

        if path.exists() {
            let file = File::open(path)?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record = parse_line(&line)?;
                store.records.insert(record.key.clone(), record);
            }
        }

        Ok(store)
    }

    /// Atomically saves the store to disk.
    ///
    /// Uses atomic write with rename: writes to a temp file, fsyncs, then renames.
    /// This ensures readers always see a complete file.
    pub fn save(&self) -> Result<(), HistoryError> {
        let temp_path = self.path.with_extension("jsonl.tmp");

        // Sort records by key for deterministic output
        let mut records: Vec<_> = self.records.values().collect();
        records.sort_by(|a, b| a.key.cmp(&b.key));

        {
            let mut file = File::create(&temp_path)?;
            for record in records {
                let line = serialize_record(record)?;
                writeln!(file, "{}", line)?;
            }
            file.sync_all()?;
        }

        std::fs::rename(&temp_path, &self.path)?;
        Ok(())
    }
}

/// Computes duration percentiles from a set of samples.
///
/// Returns `None` if fewer than 5 samples are available, since percentile
/// estimates are unreliable with too few data points.
fn compute_percentiles(samples: &[CompactSample]) -> Option<DurationStats> {
    if samples.len() < 5 {
        return None;
    }

    let mut durations: Vec<f64> = samples.iter().map(|s| s.2).collect();
    durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    Some(DurationStats {
        p50_secs: percentile(&durations, 50),
        p75_secs: percentile(&durations, 75),
        p90_secs: percentile(&durations, 90),
        p95_secs: percentile(&durations, 95),
    })
}

/// Computes the p-th percentile from a sorted slice of values.
fn percentile(sorted: &[f64], p: usize) -> f64 {
    let idx = (p as f64 / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

impl TestHistoryStore for JsonlHistoryStore {
    fn get_stats(&self, config: &str, test_id: &str) -> Option<TestStatistics> {
        let key = (config.to_string(), test_id.to_string());
        let record = self.records.get(&key)?;

        let failure_rate = if record.values.total_attempts > 0 {
            record.values.total_failures as f64 / record.values.total_attempts as f64
        } else {
            0.0
        };

        let success_stats = compute_percentiles(&record.values.ok);
        let failure_stats = compute_percentiles(&record.values.fail);

        // Derive last_attempt_ms from newest timestamp in either reservoir
        let ok_newest = record.values.ok.iter().map(|s| s.1).max();
        let fail_newest = record.values.fail.iter().map(|s| s.1).max();
        let last_attempt_ms = ok_newest.into_iter().chain(fail_newest).max().unwrap_or(0);

        Some(TestStatistics {
            test_id: test_id.to_string(),
            config: config.to_string(),
            total_attempts: record.values.total_attempts,
            total_failures: record.values.total_failures,
            failure_rate,
            duration: OutcomeStats {
                success: success_stats,
                failure: failure_stats,
            },
            last_attempt_ms,
            last_run_id: record.values.last_run.clone(),
        })
    }

    fn get_all_stats(&self, config: &str) -> Vec<TestStatistics> {
        self.records
            .keys()
            .filter(|(c, _)| c == config)
            .filter_map(|(c, t)| self.get_stats(c, t))
            .collect()
    }

    fn flakiest_tests(&self, config: &str, limit: usize) -> Vec<TestStatistics> {
        let mut stats = self.get_all_stats(config);
        stats.sort_by(|a, b| {
            b.failure_rate
                .partial_cmp(&a.failure_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        stats.truncate(limit);
        stats
    }

    fn slowest_tests(&self, config: &str, limit: usize) -> Vec<TestStatistics> {
        let mut stats = self.get_all_stats(config);
        stats.sort_by(|a, b| {
            let a_p50 = a
                .duration
                .success
                .as_ref()
                .map(|d| d.p50_secs)
                .unwrap_or(0.0);
            let b_p50 = b
                .duration
                .success
                .as_ref()
                .map(|d| d.p50_secs)
                .unwrap_or(0.0);
            b_p50
                .partial_cmp(&a_p50)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        stats.truncate(limit);
        stats
    }

    fn last_run_failures(&self, config: &str) -> Vec<String> {
        // Find max(last_run) across all tests for this config
        let latest_run = self
            .records
            .iter()
            .filter(|((c, _), _)| c == config)
            .map(|(_, r)| &r.values.last_run)
            .max();

        let Some(latest_run) = latest_run else {
            return Vec::new();
        };

        // Return test IDs where latest_run appears in fail reservoir
        self.records
            .iter()
            .filter(|((c, _), _)| c == config)
            .filter(|(_, r)| r.values.fail.iter().any(|s| &s.0 == latest_run))
            .map(|((_, test_id), _)| test_id.clone())
            .collect()
    }

    fn expected_duration(&self, config: &str, test_id: &str) -> Duration {
        // Try test-specific weighted P75
        if let Some(stats) = self.get_stats(config, test_id) {
            let ok_p75 = stats.duration.success.as_ref().map(|d| d.p75_secs);
            let fail_p75 = stats.duration.failure.as_ref().map(|d| d.p75_secs);

            match (ok_p75, fail_p75) {
                (Some(ok), Some(fail)) => {
                    let weighted = (1.0 - stats.failure_rate) * ok + stats.failure_rate * fail;
                    return Duration::from_secs_f64(weighted);
                }
                (Some(ok), None) => return Duration::from_secs_f64(ok),
                (None, Some(fail)) => return Duration::from_secs_f64(fail),
                (None, None) => {}
            }
        }

        // Fallback: group average
        let all_stats = self.get_all_stats(config);
        if !all_stats.is_empty() {
            let sum: f64 = all_stats
                .iter()
                .filter_map(|s| s.duration.success.as_ref().map(|d| d.p75_secs))
                .sum();
            let count = all_stats
                .iter()
                .filter(|s| s.duration.success.is_some())
                .count();
            if count > 0 {
                return Duration::from_secs_f64(sum / count as f64);
            }
        }

        // Final fallback: default
        Duration::from_secs_f64(self.default_duration_secs)
    }

    fn record_results(&mut self, results: &[TestAttemptResult]) -> Result<(), HistoryError> {
        for result in results {
            let key = (result.config.clone(), result.test_id.clone());

            let record = self
                .records
                .entry(key.clone())
                .or_insert_with(|| HistoryRecord {
                    key,
                    values: TestValues {
                        total_attempts: 0,
                        total_failures: 0,
                        last_run: String::new(),
                        ok: Vec::new(),
                        fail: Vec::new(),
                    },
                });

            // Update counters
            record.values.total_attempts += 1;
            if !result.passed {
                record.values.total_failures += 1;
            }
            record.values.last_run.clone_from(&result.run_id);

            // Create sample
            let sample = Sample {
                run_id: result.run_id.clone(),
                timestamp_ms: result.timestamp_ms,
                duration_secs: result.duration_secs,
            };

            // Insert into appropriate reservoir
            let target = if result.passed {
                &mut record.values.ok
            } else {
                &mut record.values.fail
            };

            // Build a WeightedReservoir from the compact samples, insert, then convert back
            let mut reservoir = WeightedReservoir::with_capacity(self.reservoir_size);
            for cs in target.iter() {
                reservoir.insert(Sample::from(cs.clone()));
            }
            reservoir.insert(sample);

            // Convert back to compact samples
            *target = reservoir
                .samples()
                .iter()
                .map(CompactSample::from)
                .collect();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_load_empty_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let store = JsonlHistoryStore::load(&path, 20, 1.0)?;
        assert!(store.records.is_empty());
        Ok(())
    }

    #[test]
    fn test_record_and_save() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");

        let mut store = JsonlHistoryStore::new(path.clone(), 20, 1.0);
        store.record_results(&[TestAttemptResult {
            config: "test.toml".into(),
            test_id: "test::foo".into(),
            run_id: "abc".into(),
            passed: true,
            duration_secs: 1.5,
            timestamp_ms: 1000,
        }])?;

        store.save()?;

        // Reload and verify
        let store2 = JsonlHistoryStore::load(&path, 20, 1.0)?;
        let stats = store2
            .get_stats("test.toml", "test::foo")
            .ok_or("expected stats to exist")?;
        assert_eq!(stats.total_attempts, 1);
        assert_eq!(stats.total_failures, 0);
        Ok(())
    }

    #[test]
    fn test_expected_duration_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let store = JsonlHistoryStore::new(path, 20, 2.5);

        // No history, should return default
        let duration = store.expected_duration("config.toml", "unknown::test");
        assert_eq!(duration, Duration::from_secs_f64(2.5));
        Ok(())
    }

    #[test]
    fn test_record_failure() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");

        let mut store = JsonlHistoryStore::new(path.clone(), 20, 1.0);
        store.record_results(&[
            TestAttemptResult {
                config: "test.toml".into(),
                test_id: "test::bar".into(),
                run_id: "run1".into(),
                passed: true,
                duration_secs: 1.0,
                timestamp_ms: 1000,
            },
            TestAttemptResult {
                config: "test.toml".into(),
                test_id: "test::bar".into(),
                run_id: "run2".into(),
                passed: false,
                duration_secs: 2.0,
                timestamp_ms: 2000,
            },
        ])?;

        let stats = store
            .get_stats("test.toml", "test::bar")
            .ok_or("expected stats to exist")?;
        assert_eq!(stats.total_attempts, 2);
        assert_eq!(stats.total_failures, 1);
        assert!((stats.failure_rate - 0.5).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_get_all_stats() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");

        let mut store = JsonlHistoryStore::new(path, 20, 1.0);
        store.record_results(&[
            TestAttemptResult {
                config: "config1.toml".into(),
                test_id: "test::a".into(),
                run_id: "run1".into(),
                passed: true,
                duration_secs: 1.0,
                timestamp_ms: 1000,
            },
            TestAttemptResult {
                config: "config1.toml".into(),
                test_id: "test::b".into(),
                run_id: "run1".into(),
                passed: true,
                duration_secs: 2.0,
                timestamp_ms: 1001,
            },
            TestAttemptResult {
                config: "config2.toml".into(),
                test_id: "test::c".into(),
                run_id: "run1".into(),
                passed: true,
                duration_secs: 3.0,
                timestamp_ms: 1002,
            },
        ])?;

        let stats1 = store.get_all_stats("config1.toml");
        assert_eq!(stats1.len(), 2);

        let stats2 = store.get_all_stats("config2.toml");
        assert_eq!(stats2.len(), 1);

        Ok(())
    }

    #[test]
    fn test_flakiest_tests() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");

        let mut store = JsonlHistoryStore::new(path, 20, 1.0);

        // Create tests with different failure rates
        // test::flaky: 2 failures / 4 attempts = 50%
        // test::stable: 0 failures / 4 attempts = 0%
        for i in 0..4 {
            store.record_results(&[
                TestAttemptResult {
                    config: "test.toml".into(),
                    test_id: "test::flaky".into(),
                    run_id: format!("run{}", i),
                    passed: i % 2 == 0, // fails on odd runs
                    duration_secs: 1.0,
                    timestamp_ms: i as u64 * 1000,
                },
                TestAttemptResult {
                    config: "test.toml".into(),
                    test_id: "test::stable".into(),
                    run_id: format!("run{}", i),
                    passed: true,
                    duration_secs: 1.0,
                    timestamp_ms: i as u64 * 1000 + 1,
                },
            ])?;
        }

        let flaky = store.flakiest_tests("test.toml", 10);
        assert_eq!(flaky.len(), 2);
        assert_eq!(flaky[0].test_id, "test::flaky");
        assert!((flaky[0].failure_rate - 0.5).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_last_run_failures() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");

        let mut store = JsonlHistoryStore::new(path, 20, 1.0);

        // First run: test::a passes, test::b fails
        store.record_results(&[
            TestAttemptResult {
                config: "test.toml".into(),
                test_id: "test::a".into(),
                run_id: "run1".into(),
                passed: true,
                duration_secs: 1.0,
                timestamp_ms: 1000,
            },
            TestAttemptResult {
                config: "test.toml".into(),
                test_id: "test::b".into(),
                run_id: "run1".into(),
                passed: false,
                duration_secs: 1.0,
                timestamp_ms: 1001,
            },
        ])?;

        // Second run: test::a fails, test::b passes
        store.record_results(&[
            TestAttemptResult {
                config: "test.toml".into(),
                test_id: "test::a".into(),
                run_id: "run2".into(),
                passed: false,
                duration_secs: 1.0,
                timestamp_ms: 2000,
            },
            TestAttemptResult {
                config: "test.toml".into(),
                test_id: "test::b".into(),
                run_id: "run2".into(),
                passed: true,
                duration_secs: 1.0,
                timestamp_ms: 2001,
            },
        ])?;

        let failures = store.last_run_failures("test.toml");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0], "test::a");
        Ok(())
    }

    /// Helper: record N ok samples for a test with sequential durations starting at `base_secs`.
    fn record_ok_samples(
        store: &mut JsonlHistoryStore,
        config: &str,
        test_id: &str,
        count: usize,
        base_secs: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let results: Vec<TestAttemptResult> = (0..count)
            .map(|i| TestAttemptResult {
                config: config.into(),
                test_id: test_id.into(),
                run_id: format!("{test_id}-ok-{i}"),
                passed: true,
                duration_secs: base_secs + i as f64,
                timestamp_ms: (i as u64 + 1) * 1000,
            })
            .collect();
        store.record_results(&results)?;
        Ok(())
    }

    /// Helper: record N fail samples for a test with sequential durations starting at `base_secs`.
    fn record_fail_samples(
        store: &mut JsonlHistoryStore,
        config: &str,
        test_id: &str,
        count: usize,
        base_secs: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let results: Vec<TestAttemptResult> = (0..count)
            .map(|i| TestAttemptResult {
                config: config.into(),
                test_id: test_id.into(),
                run_id: format!("{test_id}-fail-{i}"),
                passed: false,
                duration_secs: base_secs + i as f64,
                timestamp_ms: (i as u64 + 1) * 2000,
            })
            .collect();
        store.record_results(&results)?;
        Ok(())
    }

    #[test]
    fn test_get_scheduling_durations_populated_store() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let mut store = JsonlHistoryStore::new(path, 20, 1.0);

        // Record 5 ok samples for two tests under config1
        record_ok_samples(&mut store, "config1.toml", "test::alpha", 5, 1.0)?;
        record_ok_samples(&mut store, "config1.toml", "test::beta", 5, 10.0)?;
        // Record 5 ok samples for one test under config2
        record_ok_samples(&mut store, "config2.toml", "test::gamma", 5, 100.0)?;

        let durations = store.get_scheduling_durations("config1.toml");

        // Should contain exactly the two config1 tests
        assert_eq!(durations.len(), 2);
        assert!(durations.contains_key("test::alpha"));
        assert!(durations.contains_key("test::beta"));
        assert!(!durations.contains_key("test::gamma"));

        // Durations should be non-zero
        let alpha_dur = durations["test::alpha"];
        let beta_dur = durations["test::beta"];
        assert!(alpha_dur > Duration::ZERO);
        assert!(beta_dur > Duration::ZERO);

        // test::alpha has sorted durations [1,2,3,4,5], P75 = sorted[3] = 4.0
        assert_eq!(alpha_dur, Duration::from_secs_f64(4.0));
        // test::beta has sorted durations [10,11,12,13,14], P75 = sorted[3] = 13.0
        assert_eq!(beta_dur, Duration::from_secs_f64(13.0));

        Ok(())
    }

    #[test]
    fn test_get_scheduling_durations_empty_store() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");

        // Test with new() (no file)
        let store = JsonlHistoryStore::new(path.clone(), 20, 1.0);
        let durations = store.get_scheduling_durations("anything.toml");
        assert!(durations.is_empty());

        // Test with load() from nonexistent path
        let store2 = JsonlHistoryStore::load(&path, 20, 1.0)?;
        let durations2 = store2.get_scheduling_durations("anything.toml");
        assert!(durations2.is_empty());

        Ok(())
    }

    #[test]
    fn test_expected_duration_weighted_p75() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let mut store = JsonlHistoryStore::new(path, 20, 99.0);

        // Record 5 ok samples: durations [1, 2, 3, 4, 5] -> P75 = sorted[3] = 4.0
        record_ok_samples(&mut store, "cfg.toml", "test::mixed", 5, 1.0)?;
        // Record 5 fail samples: durations [10, 11, 12, 13, 14] -> P75 = sorted[3] = 13.0
        record_fail_samples(&mut store, "cfg.toml", "test::mixed", 5, 10.0)?;

        // failure_rate = 5 / 10 = 0.5
        // weighted = (1 - 0.5) * 4.0 + 0.5 * 13.0 = 2.0 + 6.5 = 8.5
        let duration = store.expected_duration("cfg.toml", "test::mixed");
        let expected = Duration::from_secs_f64(0.5 * 4.0 + 0.5 * 13.0);
        assert_eq!(duration, expected);

        Ok(())
    }

    #[test]
    fn test_expected_duration_ok_only() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let mut store = JsonlHistoryStore::new(path, 20, 99.0);

        // 5 ok samples: durations [2, 3, 4, 5, 6] -> P75 = sorted[3] = 5.0
        record_ok_samples(&mut store, "cfg.toml", "test::ok_only", 5, 2.0)?;
        // Only 3 fail samples (<5), so fail percentiles are None
        record_fail_samples(&mut store, "cfg.toml", "test::ok_only", 3, 50.0)?;

        // Should hit (Some(ok), None) branch -> returns ok P75 = 5.0
        let duration = store.expected_duration("cfg.toml", "test::ok_only");
        assert_eq!(duration, Duration::from_secs_f64(5.0));

        Ok(())
    }

    #[test]
    fn test_expected_duration_group_average_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let mut store = JsonlHistoryStore::new(path, 20, 99.0);

        // Two tests with enough data for P75 computation
        // test::fast: durations [1, 2, 3, 4, 5] -> P75 = 4.0
        record_ok_samples(&mut store, "cfg.toml", "test::fast", 5, 1.0)?;
        // test::slow: durations [10, 11, 12, 13, 14] -> P75 = 13.0
        record_ok_samples(&mut store, "cfg.toml", "test::slow", 5, 10.0)?;

        // test::sparse has only 2 ok samples (<5) and 0 fail -> both percentiles None
        record_ok_samples(&mut store, "cfg.toml", "test::sparse", 2, 50.0)?;

        // Group average of tests with P75: (4.0 + 13.0) / 2 = 8.5
        let duration = store.expected_duration("cfg.toml", "test::sparse");
        assert_eq!(duration, Duration::from_secs_f64(8.5));

        Ok(())
    }

    #[test]
    fn test_expected_duration_fewer_than_5_samples() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let default_secs = 7.5;
        let mut store = JsonlHistoryStore::new(path, 20, default_secs);

        // Record 3 ok samples (fewer than 5) for the only test in this config
        record_ok_samples(&mut store, "solo.toml", "test::few", 3, 1.0)?;

        // No other tests in config -> group average has count=0 -> falls to default
        let duration = store.expected_duration("solo.toml", "test::few");
        assert_eq!(duration, Duration::from_secs_f64(default_secs));

        Ok(())
    }

    // --- Integration tests: history store -> scheduler read pathway ---

    use crate::framework::TestRecord;
    use crate::orchestrator::scheduler::Scheduler;

    /// Drains all initial batches from a scheduler, returning each batch's test IDs.
    async fn drain_batch_ids(scheduler: &Scheduler) -> Vec<Vec<String>> {
        let n = scheduler.batch_count();
        let mut batches = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(batch) = scheduler.pop().await {
                batches.push(batch.tests.iter().map(|t| t.id().to_string()).collect());
            }
        }
        batches
    }

    #[tokio::test]
    async fn test_history_durations_change_scheduler_batching()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let mut store = JsonlHistoryStore::new(path, 20, 1.0);

        // Record >=5 ok samples so P75 is computed for each test.
        // "test::slow": durations [10, 11, 12, 13, 14] -> P75 = 13.0
        record_ok_samples(&mut store, "test.toml", "test::slow", 5, 10.0)?;
        // "test::medium": durations [5, 6, 7, 8, 9] -> P75 = 8.0
        record_ok_samples(&mut store, "test.toml", "test::medium", 5, 5.0)?;
        // "test::fast": durations [1, 2, 3, 4, 5] -> P75 = 4.0
        record_ok_samples(&mut store, "test.toml", "test::fast", 5, 1.0)?;

        let durations = store.get_scheduling_durations("test.toml");
        assert_eq!(durations.len(), 3);

        // Build TestInstances
        let records = [
            TestRecord::new("test::slow", "group"),
            TestRecord::new("test::medium", "group"),
            TestRecord::new("test::fast", "group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        // Scheduler WITH history durations (max_parallel=2)
        let with_history = Scheduler::new(2, &tests, &durations, &HashMap::new());
        let batches_with = drain_batch_ids(&with_history).await;

        // Scheduler WITHOUT history (empty durations, all tests use 1s default)
        let without_history = Scheduler::new(2, &tests, &HashMap::new(), &HashMap::new());
        let batches_without = drain_batch_ids(&without_history).await;

        // With history: LPT assigns slow (13s) alone, medium (8s) + fast (4s) together
        // Batch 0 (heaviest): [test::slow] = 13s
        // Batch 1: [test::medium, test::fast] = 12s
        assert_eq!(batches_with.len(), 2);
        assert_eq!(batches_with[0].len(), 1);
        assert!(batches_with[0].contains(&"test::slow".to_string()));
        assert_eq!(batches_with[1].len(), 2);

        // Without history: all 3 tests have the same 1s default duration.
        // LPT with equal durations assigns first to batch 0, second to batch 1,
        // third to the lighter batch. The distribution differs from the history case.
        // Verify the batch sizes differ from the history-informed case.
        let sizes_with: Vec<usize> = batches_with.iter().map(|b| b.len()).collect();
        let sizes_without: Vec<usize> = batches_without.iter().map(|b| b.len()).collect();
        // With history: [1, 2]. Without history: could be [2, 1] or [1, 2] but the
        // composition differs -- the slow test is NOT isolated in its own batch.
        // The key assertion: with history, the heaviest batch has exactly 1 test (slow).
        // Without history (all equal), the heaviest batch has 2 tests.
        assert_eq!(sizes_with, vec![1, 2]);
        assert_eq!(sizes_without, vec![2, 1]);

        Ok(())
    }

    #[tokio::test]
    async fn test_partial_history_uses_defaults_for_unknown_tests()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("history.jsonl");
        let mut store = JsonlHistoryStore::new(path, 20, 1.0);

        // Record history for 2 out of 4 tests (>=5 ok samples each)
        // "test::known_slow": durations [20, 21, 22, 23, 24] -> P75 = 23.0
        record_ok_samples(&mut store, "test.toml", "test::known_slow", 5, 20.0)?;
        // "test::known_fast": durations [1, 2, 3, 4, 5] -> P75 = 4.0
        record_ok_samples(&mut store, "test.toml", "test::known_fast", 5, 1.0)?;

        let durations = store.get_scheduling_durations("test.toml");
        // Should contain only the 2 known tests
        assert_eq!(durations.len(), 2);
        assert!(durations.contains_key("test::known_slow"));
        assert!(durations.contains_key("test::known_fast"));
        assert!(!durations.contains_key("test::unknown_a"));
        assert!(!durations.contains_key("test::unknown_b"));

        // Build 4 TestInstances
        let records = [
            TestRecord::new("test::known_slow", "group"),
            TestRecord::new("test::known_fast", "group"),
            TestRecord::new("test::unknown_a", "group"),
            TestRecord::new("test::unknown_b", "group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        // Scheduler with partial durations (max_parallel=2)
        // known_slow=23s, known_fast=4s, unknown_a=1s default, unknown_b=1s default
        let scheduler = Scheduler::new(2, &tests, &durations, &HashMap::new());
        let batches = drain_batch_ids(&scheduler).await;

        assert_eq!(batches.len(), 2);
        // LPT: known_slow (23s) -> batch 0
        //       known_fast (4s) -> batch 1
        //       unknown_b (1s)  -> batch 1 (lighter)
        //       unknown_a (1s)  -> batch 1 (still lighter than batch 0)
        // Batch 0 (heaviest): [known_slow] = 23s
        // Batch 1: [known_fast, unknown_b, unknown_a] = 6s
        assert_eq!(batches[0].len(), 1);
        assert!(batches[0].contains(&"test::known_slow".to_string()));
        assert_eq!(batches[1].len(), 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_empty_store_produces_empty_durations() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let nonexistent_path = dir.path().join("does_not_exist.jsonl");

        // Load from nonexistent file -> empty store
        let store = JsonlHistoryStore::load(&nonexistent_path, 20, 1.0)?;
        let durations = store.get_scheduling_durations("any.toml");
        assert!(durations.is_empty());

        // Scheduler with empty durations still works (all tests use 1s default)
        let records = [
            TestRecord::new("test::a", "group"),
            TestRecord::new("test::b", "group"),
            TestRecord::new("test::c", "group"),
        ];
        let tests: Vec<_> = records.iter().map(|r| r.test()).collect();

        let scheduler = Scheduler::new(2, &tests, &durations, &HashMap::new());
        assert_eq!(scheduler.batch_count(), 2);

        let batches = drain_batch_ids(&scheduler).await;
        // All 3 tests should be scheduled across the 2 batches
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, 3);

        Ok(())
    }
}
