//! Parity test: single-pass partition vs legacy per-group pytest discovery.
//!
//! For several realistic group configurations this runs BOTH discovery paths
//! against `examples/tests` and asserts that each group's sorted test IDs are
//! byte-identical:
//!
//! * legacy per-group [`TestFramework::discover`] — one collection per group, and
//! * single-pass [`PytestFramework::discover_all_groups`] — one pooled collection
//!   plus one collection per fallback group.
//!
//! The scenarios cover `-m`, `-k`, `-m`+`-k`, `--deselect`, `--ignore` of a
//! subdirectory, `--ignore-glob`, empty (match-all) filters, common-filter
//! hoisting (the same `-m` in every group), a group whose filter carries a
//! token the single-pass router does not model (`-p no:cacheprovider`) and which
//! both paths therefore route through legacy collection, and a framework-level
//! `discovery_args` that must narrow the collected set identically on both paths
//! (guarding the single-pass path against dropping `discovery_args`).
//!
//! Like `tests/offload_partition_plugin.rs`, the test skips gracefully
//! (returning `Ok(())`) when `uv`/pytest is unavailable, so the Rust suite stays
//! green on machines without a Python toolchain; it only fails when pytest is
//! available but the two paths disagree.

use std::collections::{BTreeMap, HashMap};
use std::process::Command;

use anyhow::Result;

use offload::TestFramework;
use offload::TestRecord;
use offload::config::{GroupConfig, PytestFrameworkConfig};
use offload::framework::pytest::PytestFramework;

/// Directory of fixture tests collected by both discovery paths.
const TEST_PATH: &str = "examples/tests";

/// Whether `program args...` runs and exits successfully, from the repo root.
fn command_succeeds(program: &str, args: &[&str]) -> bool {
    match Command::new(program)
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
    {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Whether pytest can be launched via `uv run --with=pytest`.
///
/// Mirrors the skip guard in `tests/offload_partition_plugin.rs`: a missing
/// Python toolchain makes the parity test skip rather than fail.
fn pytest_available() -> bool {
    command_succeeds("uv", &["--version"])
        && command_succeeds("uv", &["run", "--with=pytest", "pytest", "--version"])
}

/// Build the pytest framework shared by both discovery paths, optionally
/// setting the framework-level `discovery_args`.
fn build_framework_with(discovery_args: Option<&str>) -> Result<PytestFramework> {
    let config = PytestFrameworkConfig {
        command: "uv run --with=pytest pytest".into(),
        paths: Some(vec![TEST_PATH.into()]),
        discovery_args: discovery_args.map(str::to_string),
        ..Default::default()
    };
    Ok(PytestFramework::new(config)?)
}

/// A `(name, GroupConfig)` pair carrying only a filter string.
fn group(name: &str, filters: &str) -> (String, GroupConfig) {
    (
        name.to_string(),
        GroupConfig {
            filters: filters.to_string(),
            ..Default::default()
        },
    )
}

/// Legacy discovery: one `discover` call per group, sorted IDs keyed by group.
async fn legacy_by_group(
    framework: &PytestFramework,
    groups: &HashMap<String, GroupConfig>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    for (name, cfg) in groups {
        let records = framework.discover(&[], &cfg.filters, name).await?;
        let mut ids: Vec<String> = records.into_iter().map(|record| record.id).collect();
        ids.sort();
        out.insert(name.clone(), ids);
    }
    Ok(out)
}

/// Bucket single-pass records into sorted per-group ID lists.
///
/// Group names are seeded from the config so a group that selects nothing still
/// appears as an empty list, matching the legacy map's one-entry-per-group shape.
fn bucket_by_group(
    records: Vec<TestRecord>,
    group_names: impl Iterator<Item = String>,
) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> =
        group_names.map(|name| (name, Vec::new())).collect();
    for record in records {
        out.entry(record.group).or_default().push(record.id);
    }
    for ids in out.values_mut() {
        ids.sort();
    }
    out
}

/// Run both discovery paths for `groups` and assert byte-identical per-group IDs.
async fn assert_group_parity(groups: HashMap<String, GroupConfig>) -> Result<()> {
    assert_group_parity_with(groups, None).await
}

/// Run both discovery paths for `groups` under the given framework-level
/// `discovery_args` and assert byte-identical per-group IDs.
async fn assert_group_parity_with(
    groups: HashMap<String, GroupConfig>,
    discovery_args: Option<&str>,
) -> Result<()> {
    if !pytest_available() {
        eprintln!("skipping discovery parity test: pytest unavailable via `uv run --with=pytest`");
        return Ok(());
    }

    let framework = build_framework_with(discovery_args)?;

    let legacy = legacy_by_group(&framework, &groups).await?;
    let single_records = framework.discover_all_groups(&groups).await?;
    let single = bucket_by_group(single_records, groups.keys().cloned());

    let discovered: usize = legacy.values().map(|ids| ids.len()).sum();
    assert!(
        discovered > 0,
        "legacy discovery found no tests; check the fixtures or toolchain"
    );

    assert_eq!(
        single, legacy,
        "single-pass partition diverged from legacy per-group discovery"
    );
    Ok(())
}

#[tokio::test]
async fn single_pass_matches_legacy_for_filter_variants() -> Result<()> {
    let groups = HashMap::from([
        group("mark_only", "-m 'not slow'"),
        group("keyword_only", "-k test_flaky"),
        group("mark_and_keyword", "-m 'not slow' -k 'not test_flaky'"),
        group(
            "deselect_one",
            "--deselect examples/tests/test_strings.py::test_split",
        ),
        group("ignore_subdir", "--ignore examples/tests/sub"),
        group("ignore_glob_lists", "--ignore-glob '*/test_lists.py'"),
        group("match_all", ""),
    ]);
    assert_group_parity(groups).await
}

#[tokio::test]
async fn single_pass_matches_legacy_for_common_filter_hoisting() -> Result<()> {
    let groups = HashMap::from([
        group("hoist_keyword", "-m 'not slow' -k 'not test_flaky'"),
        group("hoist_ignore", "-m 'not slow' --ignore examples/tests/sub"),
        group("hoist_plain", "-m 'not slow'"),
    ]);
    assert_group_parity(groups).await
}

#[tokio::test]
async fn single_pass_matches_legacy_with_unknown_token_fallback() -> Result<()> {
    let groups = HashMap::from([
        group("unknown_token", "-p no:cacheprovider"),
        group("pool_all", ""),
    ]);
    assert_group_parity(groups).await
}

/// A framework-level `discovery_args` (`--ignore` of the `sub` subdirectory)
/// must be honored on both discovery paths. Both groups are poolable (empty
/// filter and a plain `-m`), so the single-pass pool is exercised rather than
/// the per-group fallback. Without the single-pass path applying
/// `discovery_args`, the pool would still collect `examples/tests/sub`, so its
/// two tests would appear only on the single-pass side and break parity.
#[tokio::test]
async fn single_pass_honors_discovery_args() -> Result<()> {
    let groups = HashMap::from([group("pool_all", ""), group("pool_mark", "-m 'not slow'")]);
    assert_group_parity_with(groups, Some("--ignore examples/tests/sub")).await
}
