//! Weighted reservoir sampling with temporal decay using the Efraimidis-Spirakis algorithm.
//!
//! This module provides bounded-size storage for test duration samples while
//! biasing toward recent observations. Each sample is assigned a key based on
//! its recency, and samples with higher keys are more likely to be retained.

use rand::prelude::*;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};

/// A single sample in the reservoir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Run ID this sample came from.
    pub run_id: String,
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// Duration in seconds.
    pub duration_secs: f64,
}

/// Default reservoir capacity per outcome.
const DEFAULT_CAPACITY: usize = 20;

/// Decay rate for temporal weighting.
/// With lambda = 0.1, weights decay as: 1.0, 0.90, 0.82, 0.74, 0.67, ...
const LAMBDA: f64 = 0.1;

/// Weighted reservoir with temporal decay using Efraimidis-Spirakis algorithm.
///
/// Maintains a bounded set of samples where newer samples are more likely to
/// be retained than older ones. This provides good percentile estimates while
/// keeping storage bounded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightedReservoir {
    samples: Vec<Sample>,
    capacity: usize,
}

impl WeightedReservoir {
    /// Creates a new reservoir with the default capacity (20 samples).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates a new reservoir with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Vec::new(),
            capacity,
        }
    }

    /// Insert a new sample into the reservoir.
    ///
    /// Uses Efraimidis-Spirakis weighted sampling with temporal decay.
    /// Newer samples have higher weights and are more likely to be retained.
    pub fn insert(&mut self, sample: Sample) {
        if self.samples.len() < self.capacity {
            // Room available, just add
            self.samples.push(sample);
        } else {
            // Need to potentially evict
            // Sort by timestamp descending to determine age ranks
            self.samples
                .sort_by_key(|s| std::cmp::Reverse(s.timestamp_ms));

            // Compute key for new sample (age_rank = 0, it's the newest)
            let new_key = Self::compute_key(&sample, 0);

            // Recompute keys for existing samples with updated age ranks
            let keyed: Vec<(f64, usize)> = self
                .samples
                .iter()
                .enumerate()
                .map(|(i, s)| (Self::compute_key(s, i + 1), i)) // +1 because new sample takes rank 0
                .collect();

            // Find minimum key
            let (min_idx, min_key) = keyed
                .iter()
                .min_by(|(k1, _), (k2, _)| k1.partial_cmp(k2).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(k, i)| (*i, *k))
                .unwrap_or((0, 0.0));

            // If new sample's key > min existing key, replace
            if new_key > min_key {
                self.samples[min_idx] = sample;
            }
            // else: discard new sample (it lost the lottery)
        }
    }

    /// Get all samples in the reservoir.
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// Merge another reservoir into this one.
    ///
    /// Used during git merge operations. Combines samples from both reservoirs,
    /// deduplicates by timestamp, and downsamples to capacity using weighted
    /// reservoir selection.
    pub fn merge(&mut self, other: &WeightedReservoir) {
        // Combine all samples
        let mut all_samples: Vec<Sample> = self.samples.drain(..).collect();
        all_samples.extend(other.samples.iter().cloned());

        // Deduplicate by timestamp (same timestamp = same sample)
        all_samples.sort_by_key(|s| s.timestamp_ms);
        all_samples.dedup_by_key(|s| s.timestamp_ms);

        if all_samples.len() <= self.capacity {
            self.samples = all_samples;
        } else {
            // Need to downsample using weighted reservoir selection
            // Sort by timestamp descending for age ranking
            all_samples.sort_by_key(|s| std::cmp::Reverse(s.timestamp_ms));

            // Compute keys for all samples
            let mut keyed: Vec<(f64, Sample)> = all_samples
                .into_iter()
                .enumerate()
                .map(|(i, s)| (Self::compute_key(&s, i), s))
                .collect();

            // Sort by key descending and take top capacity
            keyed.sort_by(|(k1, _), (k2, _)| {
                k2.partial_cmp(k1).unwrap_or(std::cmp::Ordering::Equal)
            });
            self.samples = keyed
                .into_iter()
                .take(self.capacity)
                .map(|(_, s)| s)
                .collect();
        }
    }

    /// Get the newest timestamp in the reservoir, if any.
    pub fn newest_timestamp(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.timestamp_ms).max()
    }

    /// Compute key for a sample using Efraimidis-Spirakis formula.
    ///
    /// `key = random^(1/weight)` where random is seeded by timestamp for determinism.
    /// Samples with higher weights produce higher keys (in expectation), making
    /// them more likely to be retained.
    fn compute_key(sample: &Sample, age_rank: usize) -> f64 {
        let weight = (-LAMBDA * age_rank as f64).exp();
        let mut rng = StdRng::seed_from_u64(sample.timestamp_ms);
        let r: f64 = rng.random();
        r.powf(1.0 / weight)
    }
}

impl Default for WeightedReservoir {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_under_capacity() {
        let mut reservoir = WeightedReservoir::with_capacity(5);
        reservoir.insert(Sample {
            run_id: "a".into(),
            timestamp_ms: 1000,
            duration_secs: 1.0,
        });
        reservoir.insert(Sample {
            run_id: "b".into(),
            timestamp_ms: 2000,
            duration_secs: 2.0,
        });
        assert_eq!(reservoir.samples().len(), 2);
    }

    #[test]
    fn test_insert_at_capacity() {
        let mut reservoir = WeightedReservoir::with_capacity(3);
        for i in 0..5 {
            reservoir.insert(Sample {
                run_id: format!("run{}", i),
                timestamp_ms: i as u64 * 1000,
                duration_secs: 1.0,
            });
        }
        assert_eq!(reservoir.samples().len(), 3);
    }

    #[test]
    fn test_deterministic_keys() {
        // Same timestamp should produce same key
        let s1 = Sample {
            run_id: "a".into(),
            timestamp_ms: 12345,
            duration_secs: 1.0,
        };
        let s2 = Sample {
            run_id: "b".into(),
            timestamp_ms: 12345,
            duration_secs: 2.0,
        };
        let k1 = WeightedReservoir::compute_key(&s1, 0);
        let k2 = WeightedReservoir::compute_key(&s2, 0);
        assert!((k1 - k2).abs() < f64::EPSILON);
    }

    #[test]
    fn test_merge_deduplication() {
        let mut r1 = WeightedReservoir::with_capacity(5);
        r1.insert(Sample {
            run_id: "a".into(),
            timestamp_ms: 1000,
            duration_secs: 1.0,
        });
        r1.insert(Sample {
            run_id: "b".into(),
            timestamp_ms: 2000,
            duration_secs: 2.0,
        });

        let mut r2 = WeightedReservoir::with_capacity(5);
        r2.insert(Sample {
            run_id: "a".into(),
            timestamp_ms: 1000,
            duration_secs: 1.0,
        }); // duplicate
        r2.insert(Sample {
            run_id: "c".into(),
            timestamp_ms: 3000,
            duration_secs: 3.0,
        });

        r1.merge(&r2);
        assert_eq!(r1.samples().len(), 3); // deduplicated
    }

    #[test]
    fn test_newest_timestamp() {
        let mut reservoir = WeightedReservoir::with_capacity(5);
        reservoir.insert(Sample {
            run_id: "a".into(),
            timestamp_ms: 1000,
            duration_secs: 1.0,
        });
        reservoir.insert(Sample {
            run_id: "b".into(),
            timestamp_ms: 3000,
            duration_secs: 2.0,
        });
        reservoir.insert(Sample {
            run_id: "c".into(),
            timestamp_ms: 2000,
            duration_secs: 3.0,
        });
        assert_eq!(reservoir.newest_timestamp(), Some(3000));
    }

    #[test]
    fn test_empty_reservoir() {
        let reservoir = WeightedReservoir::new();
        assert!(reservoir.samples().is_empty());
        assert_eq!(reservoir.newest_timestamp(), None);
    }

    #[test]
    fn test_merge_with_downsampling() {
        // Create two full reservoirs with capacity 3
        let mut r1 = WeightedReservoir::with_capacity(3);
        for i in 0..3 {
            r1.insert(Sample {
                run_id: format!("r1_{}", i),
                timestamp_ms: i as u64 * 1000,
                duration_secs: 1.0,
            });
        }

        let mut r2 = WeightedReservoir::with_capacity(3);
        for i in 3..6 {
            r2.insert(Sample {
                run_id: format!("r2_{}", i),
                timestamp_ms: i as u64 * 1000,
                duration_secs: 1.0,
            });
        }

        // Merge r2 into r1 (6 samples total, capacity 3)
        r1.merge(&r2);
        assert_eq!(r1.samples().len(), 3);
    }

    #[test]
    fn test_temporal_bias() {
        // Insert many samples and verify newer ones tend to survive
        let mut reservoir = WeightedReservoir::with_capacity(5);

        // Insert 100 samples with increasing timestamps
        for i in 0..100u64 {
            reservoir.insert(Sample {
                run_id: format!("run{}", i),
                timestamp_ms: i * 1000,
                duration_secs: 1.0,
            });
        }

        // The average timestamp should be biased toward recent values.
        // With uniform sampling from 0-99000, the average would be ~49500.
        // With temporal bias, the average should be higher.
        let avg_timestamp: f64 = reservoir
            .samples()
            .iter()
            .map(|s| s.timestamp_ms as f64)
            .sum::<f64>()
            / reservoir.samples().len() as f64;

        assert!(
            avg_timestamp > 50_000.0,
            "Expected bias toward recent samples (avg > 50000), but avg was {}",
            avg_timestamp
        );

        // The max timestamp should be recent (in the last 10% of samples).
        // Not necessarily the newest, but close to it.
        let max_timestamp = reservoir
            .samples()
            .iter()
            .map(|s| s.timestamp_ms)
            .max()
            .unwrap_or(0);

        assert!(
            max_timestamp >= 90_000,
            "Expected at least one sample from recent period, but max was {}",
            max_timestamp
        );
    }
}
