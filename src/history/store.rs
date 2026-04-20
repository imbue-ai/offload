//! JSONL-backed test history storage.
//!
//! Implements `TestHistoryStore` using a local JSONL file that can be checked
//! into source control. Maintains bounded storage via weighted reservoir sampling.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use super::jsonl::{CompactSample, TestRecord, TestValues, parse_line, serialize_record};
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
    records: HashMap<(String, String), TestRecord>,
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
                .or_insert_with(|| TestRecord {
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
}
