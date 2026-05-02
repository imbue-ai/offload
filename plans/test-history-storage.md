# Test History Storage Spec

**Status:** Complete
**Author:** Claude + Danver
**Created:** 2026-04-01

## Problem Statement

Many aspects of offload would benefit from reasoning about historical test behavior:

- What is the expected failure probability of test X?
- What is the expected runtime of test X? (median, P75, P90, P95 — for successes and failures independently)
- Which tests failed in the last run?
- Which is the flakiest test?
- Which test is the slowest?
- What's the lowest successful run-time?

Currently, offload only reads the previous `junit.xml` for duration hints. There is no cross-run persistence of test statistics.

## Goals

1. **Problem 1 - Interface**: Provide offload with a Rust trait that abstracts historical test queries. The interface should be independent of the backing implementation.

2. **Problem 2 - Local File Implementation**: Implement the interface using a local file that can be checked into source control. Recording is controlled by the `--record-history` CLI flag or the `record_history` config setting. Reading history for scheduling is always active when a `[history]` section exists.

3. **Problem 3 - CI/CD Backend (OUT OF SCOPE)**: Future work could support database backends for CI systems where results are written to a shared database rather than merged into source control. This spec explicitly does not address this—leave it underspecified.

## Design Principles

- **Small file size**: The history file should be O(number of tests), not O(runs × tests).
- **Bounded growth**: After initial population, the file size should remain approximately constant regardless of how many runs occur.
- **Merge-friendly**: Multiple developers working on branches must be able to merge their history without conflicts.
- **Fast reads**: Queries should be fast; the file is read at orchestration startup.
- **Human-readable**: Nice-to-have. JSONL format allows inspection with standard tools.
- **Diffable**: Nice-to-have. Sorted output produces meaningful git diffs.
- **Derive, don't duplicate**: Prefer computing values algorithmically from stored data over storing redundant derived fields, unless there is a strong performance reason not to.

## Key Concepts

### Test Identity

A test is uniquely identified by the tuple `(config_filename, test_id)`:

- `config_filename`: The offload configuration file (e.g., `offload-pytest-modal.toml`). This segments statistics by configuration since tests may behave differently across platforms, providers, or frameworks.
- `test_id`: The canonical test identifier as formatted by `test_id_format` in the framework config.

### Attempt vs Run

- **Attempt**: Any single execution of a test, including retries and pre-tries.
- **Run**: A complete offload invocation (may include multiple attempts per test due to retries).

For historical statistics, we count **attempts**, not runs. If a test is retried twice and fails both times, that's 2 attempts and 2 failures.

### Flakiness

Historical flakiness is simply `total_failures / total_attempts`. This differs from the current single-run flakiness detection (which marks tests as flaky if they fail then pass on retry).

## Part 1: The Query Interface

```rust
/// Statistics about a single test's historical behavior.
#[derive(Debug, Clone)]
pub struct TestStatistics {
    /// Canonical test identifier
    pub test_id: String,

    /// Configuration file these statistics are from
    pub config: String,

    /// Total number of attempts recorded
    pub total_attempts: u64,

    /// Total number of failures recorded
    pub total_failures: u64,

    /// Failure rate: total_failures / total_attempts
    pub failure_rate: f64,

    /// Duration statistics split by outcome (in seconds)
    pub duration: OutcomeStats,

    /// Timestamp of most recent attempt (Unix epoch milliseconds).
    /// Derived as max(newest_ok_timestamp, newest_fail_timestamp).
    pub last_attempt_ms: u64,

    /// Run ID of the most recent run that included this test.
    /// From the stored last_run field (not derived from reservoirs).
    /// This field is necessary because tests can be selectively run
    /// (e.g., `-k` filter), so a test may be absent from a run. The
    /// reservoir's newest sample may be from an older run that survived
    /// eviction, which would give the wrong answer.
    pub last_run_id: String,
}

#[derive(Debug, Clone)]
pub struct DurationStats {
    /// Estimated median (P50) duration
    pub p50_secs: f64,

    /// Estimated 75th percentile duration
    pub p75_secs: f64,

    /// Estimated 90th percentile duration
    pub p90_secs: f64,

    /// Estimated 95th percentile duration
    pub p95_secs: f64,
}

/// Statistics split by outcome. Each test stores separate reservoirs
/// for successes and failures, so percentiles are always computed from
/// a full N samples (when available) rather than a filtered subset.
#[derive(Debug, Clone)]
pub struct OutcomeStats {
    /// Duration statistics from the success reservoir
    pub success: Option<DurationStats>,

    /// Duration statistics from the failure reservoir
    pub failure: Option<DurationStats>,
}

/// Trait for querying historical test statistics.
///
/// Implementations may be backed by local files, databases, or return
/// default estimates when no history is available.
///
/// This trait does NOT require Send + Sync. The history store is used
/// single-threaded: loaded after the parallel test run completes,
/// mutated to record results, then saved. It is not shared across threads.
pub trait TestHistoryStore {
    /// Get statistics for a specific test.
    /// Returns None if no history exists for this test.
    fn get_stats(&self, config: &str, test_id: &str) -> Option<TestStatistics>;

    /// Get statistics for all tests matching a config.
    fn get_all_stats(&self, config: &str) -> Vec<TestStatistics>;

    /// Get the N tests with highest failure rate (all-time: total_failures / total_attempts).
    /// Uses the all-time counters rather than recent reservoir counts for
    /// statistical stability — a test with 500 failures over 10,000 runs is
    /// more reliably flaky than one with 2 failures in the last 4 runs.
    fn flakiest_tests(&self, config: &str, limit: usize) -> Vec<TestStatistics>;

    /// Get the N slowest tests for human review and prioritization.
    /// Currently ranks by success P50, but the ranking metric is an
    /// implementation detail — callers should not depend on which
    /// specific percentile or reservoir is used.
    fn slowest_tests(&self, config: &str, limit: usize) -> Vec<TestStatistics>;

    /// Get tests that failed in the most recent run.
    ///
    /// Derives the most recent run ID by finding max(last_run) across all
    /// tests for this config (no separate "last run" field is stored — this
    /// follows the design principle of deriving over duplicating).
    /// Then returns test IDs where that run ID appears in the fail reservoir.
    fn last_run_failures(&self, config: &str) -> Vec<String>;

    /// Get expected duration for scheduling purposes.
    ///
    /// Computes a weighted estimate using both reservoirs:
    ///   expected = (1 - failure_rate) * success_p75 + failure_rate * failure_p75
    ///
    /// Falls back through: test weighted P75 → group average → configurable default.
    fn expected_duration(&self, config: &str, test_id: &str) -> std::time::Duration;

    /// Record results from a completed test run.
    /// Called after each offload run completes.
    /// Takes &mut self since recording mutates internal state.
    fn record_results(&mut self, results: &[TestAttemptResult]) -> Result<(), HistoryError>;
}

/// Result of a single test attempt, used for recording.
#[derive(Debug, Clone)]
pub struct TestAttemptResult {
    pub config: String,
    pub test_id: String,
    pub run_id: String,
    pub passed: bool,
    pub duration_secs: f64,
    pub timestamp_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("No [history] section in config (required by --record-history flag)")]
    NotConfigured,
}
```

### Default/Fallback Behavior

When no history exists for a test:
- `expected_duration()` follows the fallback chain: test P75 → group average from history → configurable default
- `get_stats()` returns `None`
- When no `[history]` section exists in config, the existing `load_test_durations()` from `junit.xml` is used

## Part 2: Local File Storage

### File Format: JSONL

The history is stored as `offload-history.jsonl` in the project root. This lives outside the `.offload/` directory (which is gitignored) so it can be checked into source control without `.gitignore` exceptions.

```jsonl
{"k":["offload-pytest-modal.toml","tests/test_math.py::test_add"],"v":{...}}
{"k":["offload-pytest-modal.toml","tests/test_math.py::test_sub"],"v":{...}}
{"k":["offload-cargo-modal.toml","offload test_scheduler"],"v":{...}}
```

**Format properties:**
- One JSON object per line
- Sorted lexicographically by key `(config, test_id)` for deterministic output
- Keys are stored as `"k"` array, values as `"v"` object (compact)
- Human-readable with `jq`, `grep`, etc.
- Git diffs show which tests changed

### Per-Test Record Structure

```json
{
  "k": ["offload-pytest-modal.toml", "tests/test_math.py::test_add"],
  "v": {
    "n": 47,
    "f": 3,
    "last_run": "aKx7",
    "ok": [
      ["aKx7", 1712000000000, 2.1],
      ["aKx7", 1711999500000, 2.4],
      ["Z9pQ", 1711997500000, 2.5],
      ["Y2mR", 1711996000000, 2.2]
    ],
    "fail": [
      ["Z9pQ", 1711998000000, 2.3]
    ]
  }
}
```

**Fields:**
- `n`: Total attempt count (u64, unbounded)
- `f`: Total failure count (u64, unbounded)
- `last_run`: Run ID of the most recent run that included this test. This is stored explicitly (not derived from reservoirs) because tests can be selectively run — the newest reservoir sample may have survived eviction from an older run
- `ok`: Reservoir of recent successful samples `[run_id, timestamp_ms, duration_secs]`
- `fail`: Reservoir of recent failed samples `[run_id, timestamp_ms, duration_secs]`

Each reservoir independently holds up to N samples (default 20). Since success/failure status is determined by which reservoir a sample is in, the `passed` boolean is no longer stored per-sample — it is implicit in the reservoir membership.

**Run ID format:**
- Short, random string generated at the start of each `offload run` invocation
- 4 characters from base62 alphabet (a-zA-Z0-9) = ~14 million unique IDs
- Example: `"aKx7"`, `"Z9pQ"`
- Used to correlate samples from the same run across tests

### Bounded Growth: Weighted Reservoir Sampling

To maintain bounded storage while preserving statistical properties, we use **weighted reservoir sampling** based on the Efraimidis-Spirakis algorithm, with weights that bias toward recent samples.

**References:**
- Vitter (1985) - "Random Sampling with a Reservoir" - foundational O(N) algorithm
- Efraimidis & Spirakis (2006) - "Weighted Random Sampling with a Reservoir" - weighted extension

**Reservoir structure:**
- Two reservoirs per test: one for successes (`ok`), one for failures (`fail`)
- Each reservoir has fixed size N = 20 samples
- Each sample: `(run_id, timestamp_ms, duration_secs)` — pass/fail is implicit in reservoir membership
- This ensures up to N recent success samples and N recent failure samples are always available, regardless of the test's failure rate

**Weight function:**

Each sample is assigned a weight based on its age:

```
weight(sample) = exp(-λ * age_in_reservoir)

where:
  - λ = decay rate parameter (e.g., 0.1)
  - age_in_reservoir = number of samples inserted into the same reservoir after this one
```

With λ = 0.1, weights decay as: 1.0, 0.90, 0.82, 0.74, 0.67, ... (each ~10% smaller)

**Insertion algorithm (Efraimidis-Spirakis):**

```
Algorithm: Weighted Reservoir Sampling with Temporal Decay

Parameters:
  - N: reservoir capacity per outcome (20)
  - λ: decay rate (0.1)

On new sample S:
  1. Select target reservoir:
     - If S.passed: target = ok reservoir
     - Else: target = fail reservoir

  2. Insert into target reservoir using standard Efraimidis-Spirakis:
     a. If target.len() < N:
        - Add S to target
     b. Else:
        - Compute key for new sample: key(S) = random()^(1/weight(S))
          where weight(S) = 1.0 (newest sample)
        - For each existing sample in target:
          - Recompute weight based on current age (within this reservoir)
          - Recompute key: key(sample) = random()^(1/weight(sample))
            Note: Use deterministic RNG seeded by sample's timestamp for reproducibility
        - Find sample with minimum key
        - If key(S) > min_key: replace min-key sample with S
        - Else: discard S (rare, since new samples have high weight)

  3. Update global counters:
     - n += 1
     - f += 1 if S.passed == false
     - last_run = S.run_id
```

Note: The `min`/`max`/`min_ok` all-time counters from the previous single-reservoir design have been removed. Min/max can be derived from the reservoir samples. Since the reservoirs are bounded and biased toward recent data, these derived values reflect recent behavior rather than all-time extremes — which is the more useful metric for scheduling.

**Key insight:** The Efraimidis-Spirakis key formula `random()^(1/weight)` ensures that samples with higher weights are more likely to have higher keys, and thus more likely to survive eviction. By recomputing keys on each insertion (in memory, not stored), we achieve temporal decay without storing weights.

**Why recomputation on every insertion is necessary:** Keys must be recomputed because the weights change with every insertion — adding a new sample increments the age of all existing samples within that reservoir, changing their weights and therefore their keys. Storing precomputed keys would make them stale after the next insertion. With N=20 per reservoir this is negligible overhead (20 key computations per insertion).

**Deterministic key generation:**

To ensure reproducibility across runs (important for merging), we seed the random number generator for each sample using its timestamp (milliseconds):

```rust
fn compute_key(sample: &Sample, age_rank: usize, lambda: f64) -> f64 {
    let weight = (-lambda * age_rank as f64).exp();
    let mut rng = StdRng::seed_from_u64(sample.timestamp_ms);
    let r: f64 = rng.gen();  // uniform (0, 1)
    r.powf(1.0 / weight)
}
```

Using millisecond timestamps avoids collisions when multiple tests complete in the same second.

**Counter behavior:**

Counters `n` (total attempts) and `f` (total failures) grow unboundedly:
- They are u64, so overflow is not a practical concern
- The ratio `f/n` (failure rate) is what matters, and it remains accurate
- No decay or capping is applied

**Percentile estimation:**

Percentiles are computed independently from each reservoir:

- **Success percentiles**: From the `ok` reservoir. Used for scheduling (reflects how long a test takes when it passes).
- **Failure percentiles**: From the `fail` reservoir. Useful for understanding failure behavior (e.g., failures that crash fast vs. timeout).

Since each reservoir independently holds up to N=20 samples, percentile estimates always have a full sample set available (when the reservoir is full), regardless of the test's failure rate. This eliminates the previous concern where a high failure rate would leave too few success samples for meaningful success-only percentiles.

Computation (from a single reservoir's samples, biased toward recent):
- Sort samples by duration
- P50 = median of sorted set
- P75 = 75th percentile of sorted set
- P90 = 90th percentile of sorted set
- P95 = 95th percentile of sorted set

These are "recent percentiles" rather than all-time percentiles, which is more useful for scheduling since test performance may drift over time.

Note: P99 is not provided since N=20 samples is insufficient for meaningful P99 estimation. If a reservoir has fewer than 5 samples, percentile estimates are unreliable; callers should fall back to defaults.

### Configuration

History is a top-level config section, separate from `[report]`, since it is a cross-run concern rather than a single-run report artifact:

```toml
[history]
# When to record history after a run (default: "flag")
#   "always" — record after every run
#   "flag"   — record only when --record-history is passed on the CLI
record_history = "flag"

# Path to history file (default: "offload-history.jsonl")
path = "offload-history.jsonl"

# Reservoir size per outcome per test (default: 20)
# Each test stores up to this many success samples and this many failure samples
reservoir_size = 20

# Default duration estimate when no history or group average is available (seconds)
# Used as the final fallback in the scheduling chain: test P75 → group avg → this default
default_duration_secs = 1.0
```

**Reading vs writing:** The `record_history` setting only gates *writing* (recording results after a run). *Reading* history for LPT scheduling is always active when a `[history]` section exists in the config. This means teams benefit from smarter scheduling immediately, even if they only record history in CI.

**CLI flag: `--record-history`**

The `offload run` command accepts a `--record-history` flag:

```
offload run -c offload.toml --record-history
```

Behavior:
- If `--record-history` is passed and a `[history]` section exists: record history after the run (regardless of the `record_history` config value).
- If `--record-history` is passed but no `[history]` section exists: **error** — offload exits with a clear message explaining that `[history]` config is required.
- If `--record-history` is not passed: defer to the `record_history` config value (`"always"` records, `"flag"` does not).

To fully disable history (no reading, no writing): omit the `[history]` section entirely.

**Hardcoded parameters (not configurable):**
- Decay rate λ = 0.1 (each sample ~10% less likely to be retained than the next newer one)
- Timestamp precision: milliseconds

### Merge Driver

A custom git merge driver ensures conflict-free merges.

**Installation:**

The `offload init` command sets up the merge driver automatically:

1. Creates/updates `.gitattributes`:
```
offload-history.jsonl merge=offload-history
```

2. Creates/updates `.git/config`:
```ini
[merge "offload-history"]
    name = Offload test history merger
    driver = offload history merge %O %A %B
```

Users can also run `offload history setup-merge-driver` to configure just the merge driver without full init.

**Merge algorithm:**

```
Input: base (O), ours (A), theirs (B) - each a JSONL file

1. Parse all three into Map<(config, test_id), Record>
2. Collect all keys present in A ∪ B (union — surviving data wins)
3. For each key K:
   a. If K only in A or only in B: include as-is (even if deleted in the other branch)
   b. If K in both A and B:
      - Merge ok reservoirs: union A.ok and B.ok samples, then downsample to N
      - Merge fail reservoirs: union A.fail and B.fail samples, then downsample to N
      - Merge counters:
        n = max(A.n, B.n) + abs(A.n - B.n) * overlap_estimate
        f = similarly
      - last_run = whichever has the more recent timestamp
4. Output sorted JSONL to A (in-place)
5. Exit 0 (success, no conflicts)
```

**Rationale for "surviving data wins":** It is always safe to keep extra history data. If a test was removed from a branch, the stale history entry has zero cost (it simply won't match any future test ID). This avoids data loss from merge artifacts and simplifies the algorithm by eliminating deletion tracking.

**Reservoir merge detail:**

Each reservoir (`ok` and `fail`) is merged independently using the same procedure:

1. Combine all samples from both sides' reservoir of the same type
2. Deduplicate by timestamp (same timestamp = same sample)
3. If combined size ≤ N: keep all
4. If combined size > N: apply weighted reservoir sampling to select N samples
   - Use the same Efraimidis-Spirakis algorithm
   - Weights based on age relative to newest sample across both sets
   - Deterministic keys ensure reproducibility

**Counter merge:**

Since we can deduplicate samples by timestamp, we can compute accurate merged counters:

```
Let shared = samples present in both A and B (by timestamp)
Let only_A = samples only in A
Let only_B = samples only in B

merged.n = base.n + delta_A.n + delta_B.n
  where delta_A.n = A.n - base.n (attempts added in branch A)
  where delta_B.n = B.n - base.n (attempts added in branch B)

merged.f = similarly for failures
```

If base (O) is empty (no common ancestor), we use a heuristic based on reservoir overlap:

```
Let shared_ok = count of samples present in both A.ok and B.ok (by timestamp)
Let shared_fail = count of samples present in both A.fail and B.fail (by timestamp)
Let shared_total = shared_ok + shared_fail
Let total_samples = len(A.ok) + len(A.fail) + len(B.ok) + len(B.fail)
Let overlap_ratio = shared_total / (total_samples / 2)  # fraction of overlap
Let estimated_shared = overlap_ratio * min(A.n, B.n)

merged.n = A.n + B.n - estimated_shared
merged.f = A.f + B.f - (overlap_ratio * min(A.f, B.f))
```

Intuition: if 50% of the combined reservoir samples overlap, we estimate that ~50% of the smaller branch's total attempts are shared with the larger branch. This is imprecise but degrades gracefully — the counter will be approximately right and self-corrects over subsequent runs.

### Concurrency and Atomic Writes

To prevent corruption from concurrent writes (e.g., two `offload run` invocations finishing at the same time in a CI matrix), the `save()` method uses **atomic write with rename**:

1. Write the complete JSONL output to a temporary file in the same directory (e.g., `offload-history.jsonl.tmp.<random>`)
2. `fsync` the temporary file to ensure durability
3. Atomically rename the temporary file over `offload-history.jsonl`

Rename is atomic on POSIX filesystems, so readers will always see either the old or the new complete file, never a partial write. This does mean that if two concurrent writers race, the last rename wins and one writer's results are lost. This is acceptable for local development (rare) and CI (where results should flow through a CI/CD backend instead — see Part 3).

### Integration Points

**Recording results:**

History recording happens inside the Orchestrator, after the parallel test run completes and `MasterJunitReport::write_to_file()` is called. The config filename and history config must be passed into the Orchestrator (the Orchestrator currently only receives a `Config` struct, so the config filename needs to be threaded in as an additional parameter).

Recording is gated by the `record_history` config value and the `--record-history` CLI flag:

```rust
// In orchestrator.rs, after writing junit.xml (single-threaded context)
let should_record = match self.history_config.record_history {
    RecordHistory::Always => true,
    RecordHistory::Flag => self.record_history_flag,  // from CLI --record-history
};

if should_record {
    let mut history_store = JsonlHistoryStore::load(&self.history_config.path)?;
    // One TestAttemptResult per <testcase> element — each retry attempt
    // is a separate record, not aggregated per test ID.
    let results = extract_attempt_results(&master_report, &self.config_filename);
    history_store.record_results(&results)?;
    history_store.save()?;  // Atomic write: writes to temp file, then renames
}
```

The `--record-history` flag is validated early in `main.rs`: if the flag is passed but the config has no `[history]` section, offload exits with an error before running any tests.

The history store is used single-threaded only — it is loaded, mutated, and saved after the parallel run completes. It is not shared across worker threads and does not need `Arc<Mutex<>>` wrapping.

`extract_attempt_results` maps each `<testcase>` element in the JUnit XML to a `TestAttemptResult`. This means each retry attempt produces a separate record. For example, if a test is retried 3 times, this produces 3 `TestAttemptResult` entries (each with its own pass/fail status and duration).

**Loading for scheduling:**

Reading history for scheduling is **always active** when a `[history]` section exists in the config. This is not gated by `record_history` — even teams that only record history in CI still benefit from smarter scheduling locally.

Replace or augment `load_test_durations()`. The expected duration for a test is computed as a **weighted combination** of the success and failure reservoir P75 values:

```
expected_duration = (1 - failure_rate) * success_p75 + failure_rate * failure_p75

where:
  failure_rate = f / n  (from the global counters)
  success_p75  = P75 from the ok reservoir
  failure_p75  = P75 from the fail reservoir
```

If only one reservoir has enough samples (≥ 5), the P75 from that reservoir alone is used. This formula accounts for the fact that failures and successes often have very different durations (e.g., a test that times out on failure will have a much higher failure P75 than success P75).

**Fallback chain** when a `[history]` section exists:

1. **Weighted P75** for this specific test (formula above)
2. **Group average from history** — average weighted P75 across all tests in this config
3. **Configurable default** — a static fallback duration (e.g., 1 second)

When no `[history]` section exists, the existing `load_test_durations()` path (from `junit.xml`) is used. The previous `junit.xml` is **not** consulted as a secondary source when history is available — history fully replaces it for scheduling.

```rust
// In orchestrator.rs, during setup
let durations = if let Some(ref history_config) = config.history {
    let history = JsonlHistoryStore::load(&history_config.path)?;
    // Fallback chain: weighted test P75 → group average → default
    history.get_scheduling_durations(&config_filename, history_config.default_duration_secs)
} else {
    load_test_durations(&junit_path, test_id_format)
};
```

## Part 3: CI/CD Backend (OUT OF SCOPE)

Future work could implement `TestHistoryStore` backed by:
- A shared database (PostgreSQL, SQLite on network storage)
- A cloud service (S3 + Athena, BigQuery)
- CI-specific integrations (GitHub Actions artifacts, BuildKite metadata)

This would allow:
- Cross-machine statistics aggregation
- Historical trend analysis
- Flakiness dashboards

**This spec intentionally leaves CI/CD backends underspecified.** The interface in Part 1 is designed to accommodate such implementations, but the details are deferred.

Key considerations for future CI/CD work:
- Results written to database after each run, not merged to source
- May need authentication/authorization
- May need async writes to avoid blocking test completion
- Historical retention policies differ from local file

## Resolved Design Decisions

| Decision | Resolution |
|----------|------------|
| Reservoir structure | Two reservoirs per test: `ok` (successes) and `fail` (failures), each up to N samples |
| Reservoir size | N = 20 samples per reservoir |
| Counter behavior | Unbounded growth (u64), no decay |
| File location | `offload-history.jsonl` in project root (outside `.offload/` to avoid gitignore issues) |
| Config section | Top-level `[history]` (not nested under `[report]`); omitting section disables history entirely |
| Record gating | `record_history` enum: `"always"` (record every run) or `"flag"` (record only with `--record-history` CLI flag). Default: `"flag"` |
| CLI flag | `--record-history` on `offload run`; errors if no `[history]` section exists |
| Read vs write | Reading history for scheduling is always active when `[history]` exists; only writing is gated |
| Merge driver setup | `offload init` sets up merge driver automatically |
| Merge conflict: deleted keys | Surviving data wins — keep if present in either branch |
| Sampling algorithm | Efraimidis-Spirakis weighted reservoir sampling |
| Decay rate | λ = 0.1 (hardcoded, not configurable) |
| Percentiles | P50, P75, P90, P95 (no P99 due to small sample size) |
| Duration statistics | Separate reservoirs for success/failure; percentiles computed per-reservoir |
| `slowest_tests` ranking | Success P50 for now; encapsulated behind intent-named method so metric can change |
| `flakiest_tests` ranking | All-time failure rate (`f/n`) for statistical stability over recent-only reservoir counts |
| Timestamp precision | Milliseconds |
| Run tracking | Each sample stores run ID; `last_run_failures()` derives most recent run from max(last_run) across tests |
| Trait mutability | `record_results` takes `&mut self`; trait does not require `Send + Sync` |
| Thread safety | Single-threaded only: load after parallel run, record, save. No `Arc<Mutex<>>` needed |
| Recording location | Inside Orchestrator (config filename and history config passed in) |
| Scheduling fallback | Weighted P75 `(1-f/n)*ok_p75 + (f/n)*fail_p75` → group average → configurable default (no junit.xml fallback when `[history]` section exists) |
| Attempt mapping | One `TestAttemptResult` per `<testcase>` element (each retry is separate) |
| Concurrent writes | Atomic write via temp file + rename |
| Counter merge (no base) | Estimate shared via reservoir overlap: `overlap_ratio * min(A.n, B.n)` |
| Key recomputation | Recompute on every insertion (necessary for correctness — weights change with age) |
| CLI structure | Nested under `offload history` subcommand: `merge`, `setup-merge-driver`, `import` |
| Import command | Deferred to follow-up work |
| `last_run` field | Stored explicitly (exception to derive principle) — reservoirs can't reliably derive this because tests can be selectively run |
| `last_attempt_ms` | Derived as `max(newest_ok_timestamp, newest_fail_timestamp)` |
| Design philosophy | Prefer deriving values algorithmically over storing duplicates |

## Open Questions

None at this time.

## Appendix A: Example Queries

```rust
let store = JsonlHistoryStore::load("offload-history.jsonl")?;

// Get expected duration for scheduling
let duration = store.expected_duration("offload-pytest-modal.toml", "tests/test_slow.py::test_io");

// Find flaky tests to investigate
let flaky = store.flakiest_tests("offload-pytest-modal.toml", 10);
for test in flaky {
    println!("{}: {:.1}% failure rate", test.test_id, test.failure_rate * 100.0);
}

// Get tests that failed last run for re-run
let failures = store.last_run_failures("offload-pytest-modal.toml");
```

## Appendix B: File Size Estimates

Assumptions:
- 1000 tests
- 20 samples per reservoir × 2 reservoirs (ok + fail) = up to 40 samples per test
- ~180 bytes per sample (JSON overhead — slightly smaller than before since `passed` boolean is no longer stored per-sample)
- ~80 bytes metadata per test

Per-test size: ~7280 bytes (worst case, both reservoirs full)
Total file size: ~7.3 MB for 1000 tests (worst case)

In practice, many tests will have empty or nearly empty `fail` reservoirs (tests that rarely fail), so typical file size will be closer to ~4 MB.

With compression (gzip): ~500 KB - 1.5 MB

This is acceptable for source control.

## Appendix C: Migration Path

For existing projects:
1. Add `[history]` section to config (with `record_history = "always"` or `"flag"`)
2. First run with recording active: creates `offload-history.jsonl` with data from that run
3. Subsequent runs: accumulates history. Scheduling benefits from history immediately on next run.
4. No migration of old `junit.xml` data (start fresh)

**Deferred to follow-up work:** `offload history import junit.xml` command to seed history from existing junit files. This is not part of the initial implementation scope.

## Appendix D: Efraimidis-Spirakis Algorithm Detail

The weighted reservoir sampling algorithm works as follows:

**Goal:** Maintain a sample of size N from a stream, where each item has a weight, and the probability of an item being in the final sample is proportional to its weight.

**Algorithm:**
1. For each item i with weight w_i, compute a key: `key_i = r_i^(1/w_i)` where r_i ~ Uniform(0,1)
2. Maintain the N items with the highest keys

**Why it works:**
- Items with higher weights produce higher keys (in expectation)
- The exponent `1/w` compresses the random value for low-weight items
- Example: weight=1.0 → key ∈ (0,1), weight=0.1 → key ∈ (0,1)^10 ≈ (0, 0.1)

**For temporal decay:**
- Assign weights based on recency: `weight(i) = exp(-λ * age_i)`
- Newer items have weight ≈ 1.0, older items have weight → 0
- Recompute keys on each insertion (weights change as items age)

**Determinism:**
- Seed RNG with item's timestamp for reproducibility
- Same timestamp always produces same random value
- Critical for merge driver correctness
