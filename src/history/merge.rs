//! Git merge driver for history files.
//!
//! Implements conflict-free merging of JSONL history files. The merge algorithm
//! ensures that data from both branches is preserved, with reservoir sampling
//! used to bound the combined data when necessary.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use super::HistoryError;
use super::jsonl::{CompactSample, HistoryRecord, TestValues, parse_line, serialize_record};
use super::reservoir::{Sample, WeightedReservoir};

/// Merge three history files (base, ours, theirs) and write the result to ours.
///
/// This implements the git merge driver protocol:
/// - `base_path` is the common ancestor (%O)
/// - `ours_path` is our version (%A) and receives the merged result
/// - `theirs_path` is their version (%B)
///
/// The algorithm ensures no conflicts are possible: data present in either branch
/// survives the merge.
pub fn merge_history_files(
    base_path: &Path,
    ours_path: &Path,
    theirs_path: &Path,
    reservoir_size: usize,
) -> Result<(), HistoryError> {
    // Load all three files (base may not exist for new files)
    let base = load_records(base_path).unwrap_or_default();
    let ours = load_records(ours_path)?;
    let theirs = load_records(theirs_path)?;

    // Merge the records
    let merged = merge_records(&base, &ours, &theirs, reservoir_size);

    // Write the result atomically to ours_path
    write_records(ours_path, &merged)?;

    Ok(())
}

/// Load records from a JSONL file into a HashMap keyed by (config, test_id).
fn load_records(path: &Path) -> Result<HashMap<(String, String), HistoryRecord>, HistoryError> {
    let mut records = HashMap::new();
    if !path.exists() {
        return Ok(records);
    }
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record = parse_line(&line)?;
        records.insert(record.key.clone(), record);
    }
    Ok(records)
}

/// Merge records from base, ours, and theirs.
///
/// The merge strategy is "surviving data wins": if a test is present in either
/// branch, it is included in the result.
fn merge_records(
    base: &HashMap<(String, String), HistoryRecord>,
    ours: &HashMap<(String, String), HistoryRecord>,
    theirs: &HashMap<(String, String), HistoryRecord>,
    reservoir_size: usize,
) -> HashMap<(String, String), HistoryRecord> {
    // Collect all keys from A union B
    let mut all_keys: HashSet<_> = ours.keys().cloned().collect();
    all_keys.extend(theirs.keys().cloned());

    let mut merged = HashMap::new();
    for key in all_keys {
        let record = match (ours.get(&key), theirs.get(&key)) {
            (Some(a), Some(b)) => merge_test_records(a, b, base.get(&key), reservoir_size),
            (Some(a), None) => a.clone(),
            (None, Some(b)) => b.clone(),
            (None, None) => continue, // Should not happen given how we built all_keys
        };
        merged.insert(key, record);
    }
    merged
}

/// Merge two test records, optionally using the base for counter merging.
fn merge_test_records(
    ours: &HistoryRecord,
    theirs: &HistoryRecord,
    base: Option<&HistoryRecord>,
    reservoir_size: usize,
) -> HistoryRecord {
    // Merge ok reservoirs
    let merged_ok = merge_reservoirs(&ours.values.ok, &theirs.values.ok, reservoir_size);

    // Merge fail reservoirs
    let merged_fail = merge_reservoirs(&ours.values.fail, &theirs.values.fail, reservoir_size);

    // Merge counters
    let (merged_n, merged_f) = if let Some(b) = base {
        // Have common ancestor: use delta-based merge
        let delta_a_n = ours
            .values
            .total_attempts
            .saturating_sub(b.values.total_attempts);
        let delta_b_n = theirs
            .values
            .total_attempts
            .saturating_sub(b.values.total_attempts);
        let delta_a_f = ours
            .values
            .total_failures
            .saturating_sub(b.values.total_failures);
        let delta_b_f = theirs
            .values
            .total_failures
            .saturating_sub(b.values.total_failures);
        (
            b.values.total_attempts + delta_a_n + delta_b_n,
            b.values.total_failures + delta_a_f + delta_b_f,
        )
    } else {
        // No common ancestor: use overlap heuristic
        let shared_ok = count_shared_samples(&ours.values.ok, &theirs.values.ok);
        let shared_fail = count_shared_samples(&ours.values.fail, &theirs.values.fail);
        let total_samples = ours.values.ok.len()
            + ours.values.fail.len()
            + theirs.values.ok.len()
            + theirs.values.fail.len();
        let overlap_ratio = if total_samples > 0 {
            (shared_ok + shared_fail) as f64 * 2.0 / total_samples as f64
        } else {
            0.0
        };
        let estimated_shared_n = (overlap_ratio
            * ours.values.total_attempts.min(theirs.values.total_attempts) as f64)
            as u64;
        let estimated_shared_f = (overlap_ratio
            * ours.values.total_failures.min(theirs.values.total_failures) as f64)
            as u64;
        (
            ours.values
                .total_attempts
                .saturating_add(theirs.values.total_attempts)
                .saturating_sub(estimated_shared_n),
            ours.values
                .total_failures
                .saturating_add(theirs.values.total_failures)
                .saturating_sub(estimated_shared_f),
        )
    };

    // Pick last_run from whichever has more recent timestamp
    let ours_newest = newest_timestamp(&ours.values.ok, &ours.values.fail);
    let theirs_newest = newest_timestamp(&theirs.values.ok, &theirs.values.fail);
    let last_run = if ours_newest >= theirs_newest {
        ours.values.last_run.clone()
    } else {
        theirs.values.last_run.clone()
    };

    HistoryRecord {
        key: ours.key.clone(),
        values: TestValues {
            total_attempts: merged_n,
            total_failures: merged_f,
            last_run,
            ok: merged_ok,
            fail: merged_fail,
        },
    }
}

/// Merge two reservoir sample lists into one bounded by capacity.
///
/// Uses the reservoir's merge method which handles deduplication by timestamp.
fn merge_reservoirs(
    ours: &[CompactSample],
    theirs: &[CompactSample],
    capacity: usize,
) -> Vec<CompactSample> {
    // Build reservoirs from both sides
    let mut ours_reservoir = WeightedReservoir::with_capacity(capacity);
    for cs in ours.iter() {
        ours_reservoir.insert(Sample::from(cs.clone()));
    }

    let mut theirs_reservoir = WeightedReservoir::with_capacity(capacity);
    for cs in theirs.iter() {
        theirs_reservoir.insert(Sample::from(cs.clone()));
    }

    // Merge theirs into ours (handles deduplication and downsampling)
    ours_reservoir.merge(&theirs_reservoir);

    ours_reservoir
        .samples()
        .iter()
        .map(CompactSample::from)
        .collect()
}

/// Count samples that appear in both lists (by timestamp).
fn count_shared_samples(a: &[CompactSample], b: &[CompactSample]) -> usize {
    let a_timestamps: HashSet<_> = a.iter().map(|s| s.1).collect();
    b.iter().filter(|s| a_timestamps.contains(&s.1)).count()
}

/// Get the newest timestamp from combined ok and fail reservoirs.
fn newest_timestamp(ok: &[CompactSample], fail: &[CompactSample]) -> u64 {
    let ok_max = ok.iter().map(|s| s.1).max().unwrap_or(0);
    let fail_max = fail.iter().map(|s| s.1).max().unwrap_or(0);
    ok_max.max(fail_max)
}

/// Write records to a file atomically using temp file + rename.
fn write_records(
    path: &Path,
    records: &HashMap<(String, String), HistoryRecord>,
) -> Result<(), HistoryError> {
    let temp_path = path.with_extension("jsonl.tmp");

    // Sort by key for deterministic output
    let mut sorted: Vec<_> = records.values().collect();
    sorted.sort_by(|a, b| a.key.cmp(&b.key));

    {
        let mut file = File::create(&temp_path)?;
        for record in sorted {
            let line = serialize_record(record)?;
            writeln!(file, "{}", line)?;
        }
        file.sync_all()?;
    }

    std::fs::rename(&temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_sample(run_id: &str, timestamp_ms: u64, duration_secs: f64) -> CompactSample {
        CompactSample(run_id.to_string(), timestamp_ms, duration_secs)
    }

    fn make_record(
        config: &str,
        test_id: &str,
        attempts: u64,
        failures: u64,
        last_run: &str,
        ok: Vec<CompactSample>,
        fail: Vec<CompactSample>,
    ) -> HistoryRecord {
        HistoryRecord {
            key: (config.to_string(), test_id.to_string()),
            values: TestValues {
                total_attempts: attempts,
                total_failures: failures,
                last_run: last_run.to_string(),
                ok,
                fail,
            },
        }
    }

    #[test]
    fn test_merge_disjoint_tests() {
        let base = HashMap::new();

        let mut ours = HashMap::new();
        ours.insert(
            ("cfg".to_string(), "test_a".to_string()),
            make_record("cfg", "test_a", 10, 1, "run1", vec![], vec![]),
        );

        let mut theirs = HashMap::new();
        theirs.insert(
            ("cfg".to_string(), "test_b".to_string()),
            make_record("cfg", "test_b", 5, 0, "run2", vec![], vec![]),
        );

        let merged = merge_records(&base, &ours, &theirs, 20);

        assert_eq!(merged.len(), 2);
        assert!(merged.contains_key(&("cfg".to_string(), "test_a".to_string())));
        assert!(merged.contains_key(&("cfg".to_string(), "test_b".to_string())));
    }

    #[test]
    fn test_merge_same_test_with_base() -> Result<(), Box<dyn std::error::Error>> {
        let mut base = HashMap::new();
        base.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record("cfg", "test", 10, 2, "run0", vec![], vec![]),
        );

        let mut ours = HashMap::new();
        ours.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record("cfg", "test", 15, 3, "run1", vec![], vec![]),
        );

        let mut theirs = HashMap::new();
        theirs.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record("cfg", "test", 12, 4, "run2", vec![], vec![]),
        );

        let merged = merge_records(&base, &ours, &theirs, 20);

        let record = merged
            .get(&("cfg".to_string(), "test".to_string()))
            .ok_or("merged record should exist")?;

        // base=10, ours=15 (delta=5), theirs=12 (delta=2)
        // merged = 10 + 5 + 2 = 17
        assert_eq!(record.values.total_attempts, 17);

        // base=2, ours=3 (delta=1), theirs=4 (delta=2)
        // merged = 2 + 1 + 2 = 5
        assert_eq!(record.values.total_failures, 5);

        Ok(())
    }

    #[test]
    fn test_merge_same_test_no_base() -> Result<(), Box<dyn std::error::Error>> {
        let base = HashMap::new();

        let mut ours = HashMap::new();
        ours.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record(
                "cfg",
                "test",
                10,
                2,
                "run1",
                vec![make_sample("run1", 1000, 1.0)],
                vec![],
            ),
        );

        let mut theirs = HashMap::new();
        theirs.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record(
                "cfg",
                "test",
                10,
                2,
                "run2",
                vec![make_sample("run2", 2000, 1.5)],
                vec![],
            ),
        );

        let merged = merge_records(&base, &ours, &theirs, 20);

        let record = merged
            .get(&("cfg".to_string(), "test".to_string()))
            .ok_or("merged record should exist")?;

        // No overlap in samples, so estimates should combine totals
        // (overlap_ratio = 0, so merged = ours + theirs)
        assert_eq!(record.values.total_attempts, 20);
        assert_eq!(record.values.total_failures, 4);

        Ok(())
    }

    #[test]
    fn test_merge_reservoir_deduplication() -> Result<(), Box<dyn std::error::Error>> {
        let base = HashMap::new();

        // Both sides have the same sample (same timestamp)
        let shared_sample = make_sample("run1", 1000, 1.0);

        let mut ours = HashMap::new();
        ours.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record(
                "cfg",
                "test",
                5,
                0,
                "run1",
                vec![shared_sample.clone(), make_sample("run1", 2000, 1.1)],
                vec![],
            ),
        );

        let mut theirs = HashMap::new();
        theirs.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record(
                "cfg",
                "test",
                5,
                0,
                "run1",
                vec![shared_sample, make_sample("run2", 3000, 1.2)],
                vec![],
            ),
        );

        let merged = merge_records(&base, &ours, &theirs, 20);

        let record = merged
            .get(&("cfg".to_string(), "test".to_string()))
            .ok_or("merged record should exist")?;

        // Should have 3 unique samples (timestamp 1000 deduplicated)
        assert_eq!(record.values.ok.len(), 3);

        Ok(())
    }

    #[test]
    fn test_merge_picks_most_recent_last_run() -> Result<(), Box<dyn std::error::Error>> {
        let base = HashMap::new();

        let mut ours = HashMap::new();
        ours.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record(
                "cfg",
                "test",
                5,
                0,
                "old_run",
                vec![make_sample("old_run", 1000, 1.0)],
                vec![],
            ),
        );

        let mut theirs = HashMap::new();
        theirs.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record(
                "cfg",
                "test",
                5,
                0,
                "new_run",
                vec![make_sample("new_run", 2000, 1.0)],
                vec![],
            ),
        );

        let merged = merge_records(&base, &ours, &theirs, 20);

        let record = merged
            .get(&("cfg".to_string(), "test".to_string()))
            .ok_or("merged record should exist")?;

        // theirs has newer timestamp, so its last_run should be used
        assert_eq!(record.values.last_run, "new_run");

        Ok(())
    }

    #[test]
    fn test_merge_only_in_ours() -> Result<(), Box<dyn std::error::Error>> {
        let base = HashMap::new();

        let mut ours = HashMap::new();
        ours.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record("cfg", "test", 10, 1, "run1", vec![], vec![]),
        );

        let theirs = HashMap::new();

        let merged = merge_records(&base, &ours, &theirs, 20);

        assert_eq!(merged.len(), 1);
        let record = merged
            .get(&("cfg".to_string(), "test".to_string()))
            .ok_or("merged record should exist")?;
        assert_eq!(record.values.total_attempts, 10);

        Ok(())
    }

    #[test]
    fn test_merge_only_in_theirs() -> Result<(), Box<dyn std::error::Error>> {
        let base = HashMap::new();
        let ours = HashMap::new();

        let mut theirs = HashMap::new();
        theirs.insert(
            ("cfg".to_string(), "test".to_string()),
            make_record("cfg", "test", 10, 1, "run1", vec![], vec![]),
        );

        let merged = merge_records(&base, &ours, &theirs, 20);

        assert_eq!(merged.len(), 1);
        let record = merged
            .get(&("cfg".to_string(), "test".to_string()))
            .ok_or("merged record should exist")?;
        assert_eq!(record.values.total_attempts, 10);

        Ok(())
    }

    #[test]
    fn test_merge_history_files_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let base_path = dir.path().join("base.jsonl");
        let ours_path = dir.path().join("ours.jsonl");
        let theirs_path = dir.path().join("theirs.jsonl");

        // Write ours
        {
            let mut file = File::create(&ours_path)?;
            let record = make_record(
                "cfg",
                "test_a",
                10,
                1,
                "run1",
                vec![make_sample("run1", 1000, 1.0)],
                vec![],
            );
            writeln!(file, "{}", serialize_record(&record)?)?;
        }

        // Write theirs
        {
            let mut file = File::create(&theirs_path)?;
            let record = make_record(
                "cfg",
                "test_b",
                5,
                0,
                "run2",
                vec![make_sample("run2", 2000, 1.5)],
                vec![],
            );
            writeln!(file, "{}", serialize_record(&record)?)?;
        }

        // base doesn't exist (simulating new file)

        merge_history_files(&base_path, &ours_path, &theirs_path, 20)?;

        // Verify result was written to ours_path
        let merged = load_records(&ours_path)?;
        assert_eq!(merged.len(), 2);
        assert!(merged.contains_key(&("cfg".to_string(), "test_a".to_string())));
        assert!(merged.contains_key(&("cfg".to_string(), "test_b".to_string())));

        Ok(())
    }
}
