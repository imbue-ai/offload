//! Integration test for the bundled `offload_partition` pytest plugin.
//!
//! Runs a real `pytest --collect-only -q` with the plugin against
//! `examples/tests` and asserts the per-group partition it writes to JSON. The
//! test skips gracefully (returning `Ok(())`) when the Python toolchain (`uv`
//! or pytest) is unavailable, so the Rust suite stays green on machines without
//! Python; it only fails when pytest is available but the partition is wrong.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Directory of fixture tests collected during the run.
const TEST_PATH: &str = "examples/tests";

#[derive(Deserialize)]
struct Partition {
    groups: HashMap<String, Vec<String>>,
}

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

/// A unique path under the system temp dir, keyed by pid and wall-clock time.
fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos(),
        Err(_) => 0,
    };
    let name = format!(
        "offload_partition_{label}_{}_{nanos}.json",
        std::process::id()
    );
    std::env::temp_dir().join(name)
}

/// The node ID list for `name`, preserving the plugin's collection order.
fn group_vec(groups: &HashMap<String, Vec<String>>, name: &str) -> Result<Vec<String>> {
    groups
        .get(name)
        .cloned()
        .with_context(|| format!("plugin output missing group `{name}`"))
}

/// The node ID set for `name`.
fn group_set(groups: &HashMap<String, Vec<String>>, name: &str) -> Result<BTreeSet<String>> {
    Ok(group_vec(groups, name)?.into_iter().collect())
}

/// The subset of `all` whose node IDs begin with `prefix`.
fn nodes_with_prefix(all: &BTreeSet<String>, prefix: &str) -> BTreeSet<String> {
    all.iter()
        .filter(|node| node.starts_with(prefix))
        .cloned()
        .collect()
}

#[test]
fn partitions_collected_items_per_group() -> Result<()> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    if !command_succeeds("uv", &["--version"]) {
        eprintln!("skipping offload_partition plugin test: `uv` not found on PATH");
        return Ok(());
    }

    let scripts_dir = PathBuf::from(manifest_dir).join("scripts");
    let cfg_path = unique_temp_path("cfg");
    let out_path = unique_temp_path("out");

    let out_str = out_path
        .to_str()
        .context("temp output path is not valid UTF-8")?;
    let config = serde_json::json!({
        "groups": {
            "all": {},
            "unit": {"mark": "not slow", "keyword": "not test_flaky"},
            "slow": {"mark": "slow"},
            "flaky": {"keyword": "test_flaky"},
            "deselect_math": {"deselect": [format!("{TEST_PATH}/test_math.py")]},
            "ignore_strings": {"ignore": [format!("{TEST_PATH}/test_strings.py")]},
            "ignore_glob_lists": {"ignore_glob": ["*/test_lists.py"]},
        },
        "out": out_str,
    });
    fs::write(&cfg_path, serde_json::to_string(&config)?)
        .context("failed to write plugin config")?;

    let output = Command::new("uv")
        .args([
            "run",
            "--with=pytest",
            "pytest",
            "--collect-only",
            "-q",
            "-p",
            "offload_partition",
            TEST_PATH,
        ])
        .current_dir(manifest_dir)
        .env("PYTHONPATH", &scripts_dir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("OFFLOAD_PARTITION_CONFIG", &cfg_path)
        .output()
        .context("failed to spawn `uv run pytest`")?;

    if !out_path.exists() {
        let _ = fs::remove_file(&cfg_path);
        if command_succeeds("uv", &["run", "--with=pytest", "pytest", "--version"]) {
            return Err(anyhow::anyhow!(
                "plugin produced no output though pytest is available; stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        eprintln!("skipping offload_partition plugin test: pytest unavailable via `uv run`");
        return Ok(());
    }

    let report = fs::read_to_string(&out_path).context("failed to read plugin output")?;
    let _ = fs::remove_file(&cfg_path);
    let _ = fs::remove_file(&out_path);

    let partition: Partition = serde_json::from_str(&report).context("invalid plugin JSON")?;
    let groups = &partition.groups;

    let all_vec = group_vec(groups, "all")?;
    let all = group_set(groups, "all")?;
    let unit = group_set(groups, "unit")?;
    let slow = group_set(groups, "slow")?;
    let flaky = group_set(groups, "flaky")?;
    let deselect_math = group_set(groups, "deselect_math")?;
    let ignore_strings = group_set(groups, "ignore_strings")?;
    let ignore_glob_lists = group_set(groups, "ignore_glob_lists")?;

    assert!(!all.is_empty(), "collection produced no items");

    // The `all` group (empty filters) must reproduce, in order, exactly the node
    // IDs pytest itself prints for `--collect-only -q`.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let printed: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(TEST_PATH) && line.contains("::"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        all_vec, printed,
        "`all` group must equal pytest --collect-only order"
    );

    let quick_maffs = format!("{TEST_PATH}/test_math.py::test_quick_maffs");
    let flaky_node = format!("{TEST_PATH}/test_flaky.py::test_flaky");
    assert_eq!(slow, BTreeSet::from([quick_maffs]));
    assert_eq!(flaky, BTreeSet::from([flaky_node]));
    assert!(all.is_superset(&slow));
    assert!(all.is_superset(&flaky));

    assert_eq!(
        unit,
        &(&all - &slow) - &flaky,
        "unit must exclude slow + flaky"
    );

    let math = nodes_with_prefix(&all, &format!("{TEST_PATH}/test_math.py"));
    assert!(!math.is_empty(), "fixtures should include math tests");
    assert_eq!(deselect_math, &all - &math);

    let strings = nodes_with_prefix(&all, &format!("{TEST_PATH}/test_strings.py"));
    assert!(!strings.is_empty(), "fixtures should include string tests");
    assert_eq!(ignore_strings, &all - &strings);

    let lists = nodes_with_prefix(&all, &format!("{TEST_PATH}/test_lists.py"));
    assert!(!lists.is_empty(), "fixtures should include list tests");
    assert_eq!(ignore_glob_lists, &all - &lists);

    Ok(())
}
