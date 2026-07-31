//! Pure data types and helpers for pytest single-pass discovery.
//!
//! The `offload_partition` plugin partitions one `pytest --collect-only` run
//! into per-group node ID lists. This module owns the config it consumes, the
//! report it produces, the parsed-component-to-spec conversion, and per-group
//! routing. All process and filesystem I/O lives in the `pytest` module, so
//! everything here stays side-effect free and unit-testable without pytest.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::TestRecord;
use super::pytest_filter::{
    FilterComponent, ParseError, is_single_pass_eligible, parse_filter_string,
};
use crate::config::GroupConfig;

/// One group's filter specification for the `offload_partition` plugin.
///
/// Field semantics mirror pytest's own argparse handling so a group's members
/// match legacy per-group collection exactly: `-m`/`-k` are single-valued
/// (the last occurrence wins), while `--deselect`/`--ignore`/`--ignore-glob`
/// accumulate in order.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct GroupSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mark: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) keyword: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) deselect: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) ignore: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) ignore_glob: Vec<String>,
}

/// The config document written for the `offload_partition` plugin.
#[derive(Debug, Serialize)]
pub(crate) struct PartitionConfig {
    pub(crate) groups: BTreeMap<String, GroupSpec>,
    pub(crate) out: PathBuf,
}

/// The plugin's output: node IDs selected per group, in collection order.
#[derive(Debug, Deserialize)]
pub(crate) struct PartitionReport {
    pub(crate) groups: HashMap<String, Vec<String>>,
}

/// How a single group should be discovered.
pub(crate) enum Routing {
    /// Eligible for the single-pass pool, carrying its parsed components.
    Pool(Vec<FilterComponent>),
    /// Ineligible (contains an unsupported token); use legacy per-group collection.
    Fallback,
}

/// Decide whether a group joins the single-pass pool or falls back to legacy.
///
/// Returns `Err` only when the filter string cannot be tokenized/parsed, a
/// condition the legacy path rejects identically.
pub(crate) fn route_group(filters: &str) -> Result<Routing, ParseError> {
    let components = parse_filter_string(filters)?;
    if is_single_pass_eligible(&components) {
        Ok(Routing::Pool(components))
    } else {
        Ok(Routing::Fallback)
    }
}

/// Convert one group's parsed components into a plugin `GroupSpec`.
pub(crate) fn group_spec_from_components(components: &[FilterComponent]) -> GroupSpec {
    let mut spec = GroupSpec::default();
    for component in components {
        match component {
            FilterComponent::Mark(value) => spec.mark = Some(value.clone()),
            FilterComponent::Keyword(value) => spec.keyword = Some(value.clone()),
            FilterComponent::Deselect(value) => spec.deselect.push(value.clone()),
            FilterComponent::Ignore(value) => spec.ignore.push(value.clone()),
            FilterComponent::IgnoreGlob(value) => spec.ignore_glob.push(value.clone()),
            FilterComponent::Unknown(_) => {}
        }
    }
    spec
}

/// Collapse a pool group's parsed components to the effective filter pytest
/// would apply: only the last `Mark` and last `Keyword` survive (argparse
/// treats `-m`/`-k` as single-valued), while `Deselect`/`Ignore`/`IgnoreGlob`
/// accumulate in their original relative order.
///
/// Hoisting must run over these normalized lists so a non-last but common
/// mark/keyword can never be emitted onto the shared `--collect-only` command
/// line, where pytest's built-in deselection would pre-narrow the collection
/// ahead of the per-group plugin spec. This mirrors the last-wins rule in
/// [`group_spec_from_components`]. Pool groups contain no `Unknown` by
/// construction; any such component is dropped.
pub(crate) fn effective_components(components: &[FilterComponent]) -> Vec<FilterComponent> {
    let last_mark = components
        .iter()
        .rposition(|c| matches!(c, FilterComponent::Mark(_)));
    let last_keyword = components
        .iter()
        .rposition(|c| matches!(c, FilterComponent::Keyword(_)));

    components
        .iter()
        .enumerate()
        .filter_map(|(index, component)| match component {
            FilterComponent::Mark(_) => (Some(index) == last_mark).then(|| component.clone()),
            FilterComponent::Keyword(_) => (Some(index) == last_keyword).then(|| component.clone()),
            FilterComponent::Deselect(_)
            | FilterComponent::Ignore(_)
            | FilterComponent::IgnoreGlob(_) => Some(component.clone()),
            FilterComponent::Unknown(_) => None,
        })
        .collect()
}

/// Build tagged `TestRecord`s from a plugin report.
///
/// Each node ID becomes a record tagged with its group's `retry_count` and
/// `schedule_individual`, preserving pytest's per-group collection order. A
/// report group missing from `groups` falls back to untagged defaults, which
/// cannot happen for a pool the caller assembled from `groups`.
pub(crate) fn records_from_report(
    report: &PartitionReport,
    groups: &HashMap<String, GroupConfig>,
) -> Vec<TestRecord> {
    let mut records = Vec::new();
    for (group_name, node_ids) in &report.groups {
        let (retry_count, schedule_individual) = groups
            .get(group_name)
            .map(|cfg| (cfg.retry_count, cfg.schedule_individual))
            .unwrap_or((0, false));
        for node_id in node_ids {
            records.push(
                TestRecord::new(node_id, group_name)
                    .with_retry_count(retry_count)
                    .with_schedule_individual(schedule_individual),
            );
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::pytest_filter::hoist_common_components;

    #[test]
    fn test_group_spec_last_wins_mark_and_keyword() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m foo -m bar -k a -k b")?;
        let spec = group_spec_from_components(&components);
        assert_eq!(spec.mark.as_deref(), Some("bar"));
        assert_eq!(spec.keyword.as_deref(), Some("b"));
        Ok(())
    }

    #[test]
    fn test_effective_components_keeps_last_mark_and_keyword()
    -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m foo -m bar -k a -k b")?;
        let effective = effective_components(&components);
        assert_eq!(
            effective,
            vec![
                FilterComponent::Mark("bar".to_string()),
                FilterComponent::Keyword("b".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_effective_components_preserves_deselect_ignore_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string(
            "--deselect a.py -m foo --ignore x -m bar --ignore-glob '*_y.py' --deselect b.py",
        )?;
        let effective = effective_components(&components);
        assert_eq!(
            effective,
            vec![
                FilterComponent::Deselect("a.py".to_string()),
                FilterComponent::Ignore("x".to_string()),
                FilterComponent::Mark("bar".to_string()),
                FilterComponent::IgnoreGlob("*_y.py".to_string()),
                FilterComponent::Deselect("b.py".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_effective_hoisting_drops_non_last_common_mark() -> Result<(), Box<dyn std::error::Error>>
    {
        // Group A `-m foo -m bar`, group B `-m foo -m baz`: `foo` is common but
        // non-last. After normalization the effective marks (`bar`/`baz`) differ,
        // so nothing hoists and `foo` never reaches the shared command line.
        let a = effective_components(&parse_filter_string("-m foo -m bar")?);
        let b = effective_components(&parse_filter_string("-m foo -m baz")?);
        let hoisted = hoist_common_components(&[a, b]).ok_or("expected Some hoist")?;
        assert!(!hoisted.contains(&FilterComponent::Mark("foo".to_string())));
        assert!(hoisted.is_empty());
        Ok(())
    }

    #[test]
    fn test_effective_hoisting_hoists_last_common_mark() -> Result<(), Box<dyn std::error::Error>> {
        // Both groups `-m x -m y`: the effective mark `y` is common, so only `y`
        // hoists; the non-last `x` does not.
        let a = effective_components(&parse_filter_string("-m x -m y")?);
        let b = effective_components(&parse_filter_string("-m x -m y")?);
        let hoisted = hoist_common_components(&[a, b]).ok_or("expected Some hoist")?;
        assert_eq!(hoisted, vec![FilterComponent::Mark("y".to_string())]);
        assert!(!hoisted.contains(&FilterComponent::Mark("x".to_string())));
        Ok(())
    }

    #[test]
    fn test_effective_hoisting_drops_non_last_common_keyword()
    -> Result<(), Box<dyn std::error::Error>> {
        let a = effective_components(&parse_filter_string("-k foo -k bar")?);
        let b = effective_components(&parse_filter_string("-k foo -k baz")?);
        let hoisted = hoist_common_components(&[a, b]).ok_or("expected Some hoist")?;
        assert!(!hoisted.contains(&FilterComponent::Keyword("foo".to_string())));
        assert!(hoisted.is_empty());
        Ok(())
    }

    #[test]
    fn test_effective_hoisting_single_mark_per_group_is_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        // Realistic config: one `-m` per group. Normalization is a no-op, so
        // hoisting behaves exactly as it did over the raw components.
        let a_raw = parse_filter_string("-m 'not slow' --ignore tests/integration")?;
        let b_raw = parse_filter_string("-m 'not slow' --deselect tests/test_old.py")?;
        let a = effective_components(&a_raw);
        let b = effective_components(&b_raw);
        assert_eq!(a, a_raw);
        assert_eq!(b, b_raw);
        let hoisted = hoist_common_components(&[a, b]).ok_or("expected Some hoist")?;
        assert_eq!(hoisted, vec![FilterComponent::Mark("not slow".to_string())]);
        Ok(())
    }

    #[test]
    fn test_group_spec_accumulates_lists_in_order() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string(
            "--deselect a.py --ignore x --deselect b.py --ignore y \
             --ignore-glob '*_x.py' --ignore-glob '*_y.py'",
        )?;
        let spec = group_spec_from_components(&components);
        assert_eq!(spec.deselect, vec!["a.py".to_string(), "b.py".to_string()]);
        assert_eq!(spec.ignore, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(
            spec.ignore_glob,
            vec!["*_x.py".to_string(), "*_y.py".to_string()]
        );
        assert!(spec.mark.is_none() && spec.keyword.is_none());
        Ok(())
    }

    #[test]
    fn test_partition_config_serializes_to_plugin_contract()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut groups = BTreeMap::new();
        groups.insert("all".to_string(), group_spec_from_components(&[]));
        groups.insert(
            "unit".to_string(),
            group_spec_from_components(&parse_filter_string("-m 'not slow' -k 'not test_flaky'")?),
        );
        groups.insert(
            "deselect_math".to_string(),
            group_spec_from_components(&parse_filter_string(
                "--deselect examples/tests/test_math.py",
            )?),
        );
        let config = PartitionConfig {
            groups,
            out: PathBuf::from("/tmp/out.json"),
        };
        let value = serde_json::to_value(&config)?;
        let expected = serde_json::json!({
            "groups": {
                "all": {},
                "unit": {"mark": "not slow", "keyword": "not test_flaky"},
                "deselect_math": {"deselect": ["examples/tests/test_math.py"]},
            },
            "out": "/tmp/out.json",
        });
        assert_eq!(value, expected);
        Ok(())
    }

    #[test]
    fn test_records_from_report_tags_groups() -> Result<(), Box<dyn std::error::Error>> {
        let report: PartitionReport = serde_json::from_str(
            r#"{"groups": {"unit": ["a.py::t1", "a.py::t2"], "slow": ["b.py::t3"]}}"#,
        )?;
        let mut groups = HashMap::new();
        groups.insert(
            "unit".to_string(),
            GroupConfig {
                retry_count: 2,
                ..Default::default()
            },
        );
        groups.insert(
            "slow".to_string(),
            GroupConfig {
                retry_count: 3,
                schedule_individual: true,
                ..Default::default()
            },
        );

        let records = records_from_report(&report, &groups);
        assert_eq!(records.len(), 3);

        let unit: Vec<&TestRecord> = records.iter().filter(|r| r.group == "unit").collect();
        assert_eq!(unit.len(), 2);
        assert!(
            unit.iter()
                .all(|r| r.retry_count == 2 && !r.schedule_individual)
        );
        assert_eq!(unit[0].id, "a.py::t1");
        assert_eq!(unit[1].id, "a.py::t2");

        let slow: Vec<&TestRecord> = records.iter().filter(|r| r.group == "slow").collect();
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0].id, "b.py::t3");
        assert!(slow[0].retry_count == 3 && slow[0].schedule_individual);
        Ok(())
    }

    #[test]
    fn test_records_from_report_untracked_group_uses_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let report: PartitionReport =
            serde_json::from_str(r#"{"groups": {"ghost": ["a.py::t1"]}}"#)?;
        let groups = HashMap::new();
        let records = records_from_report(&report, &groups);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].retry_count, 0);
        assert!(!records[0].schedule_individual);
        Ok(())
    }

    #[test]
    fn test_route_group_pool_for_all_known() -> Result<(), Box<dyn std::error::Error>> {
        match route_group("-m 'not slow' --ignore tests/integration")? {
            Routing::Pool(components) => assert_eq!(components.len(), 2),
            Routing::Fallback => return Err("expected Pool routing".into()),
        }
        Ok(())
    }

    #[test]
    fn test_route_group_fallback_for_unknown_token() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            route_group("-m 'not slow' --no-cov")?,
            Routing::Fallback
        ));
        Ok(())
    }

    #[test]
    fn test_route_group_empty_is_pool() -> Result<(), Box<dyn std::error::Error>> {
        match route_group("")? {
            Routing::Pool(components) => assert!(components.is_empty()),
            Routing::Fallback => return Err("expected empty Pool routing".into()),
        }
        Ok(())
    }

    #[test]
    fn test_route_group_parse_error_is_err() {
        assert!(route_group("-m").is_err());
    }

    #[test]
    fn test_empty_filters_produce_empty_spec() {
        let spec = group_spec_from_components(&[]);
        assert_eq!(spec, GroupSpec::default());
        assert!(spec.mark.is_none() && spec.keyword.is_none());
        assert!(spec.deselect.is_empty() && spec.ignore.is_empty() && spec.ignore_glob.is_empty());
    }

    #[test]
    fn test_single_group_spec_contains_all_components() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m slow --ignore tests/x")?;
        let spec = group_spec_from_components(&components);
        assert_eq!(spec.mark.as_deref(), Some("slow"));
        assert_eq!(spec.ignore, vec!["tests/x".to_string()]);
        assert!(spec.keyword.is_none() && spec.deselect.is_empty());
        Ok(())
    }
}
