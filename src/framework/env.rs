//! Resolution of framework-level `env` and `prepend_path` configuration.
//!
//! Framework commands run in two contexts: test discovery on the local
//! machine (root = local working directory) and test execution inside a
//! sandbox (root = `OFFLOAD_ROOT`, i.e. `sandbox_project_root`). The helpers
//! here take the applicable root so callers resolve config values for the
//! context they are about to run in.

use std::collections::HashMap;
use std::path::Path;

use super::{FrameworkError, FrameworkResult};
use crate::provider::Command;

/// Substitute the `{root}` placeholder in each env value.
pub fn resolve_env(entries: &HashMap<String, String>, root: &Path) -> HashMap<String, String> {
    let root = root.to_string_lossy();
    entries
        .iter()
        .map(|(key, value)| (key.clone(), value.replace("{root}", root.as_ref())))
        .collect()
}

/// Attach framework env entries and `PATH` prepend dirs to an execution
/// command. `{root}` placeholders in env values are left unresolved —
/// providers resolve them against the sandbox project root at execution
/// time. Env entries are sorted by key so rendered commands are
/// deterministic.
pub(crate) fn attach_execution_env(
    cmd: &mut Command,
    env: &HashMap<String, String>,
    prepend_path: Option<&[String]>,
) {
    let mut entries: Vec<(&String, &String)> = env.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    for (key, value) in entries {
        cmd.env.push((key.clone(), value.clone()));
    }
    if let Some(dirs) = prepend_path {
        cmd.path_prepend.extend(dirs.iter().cloned());
    }
}

/// Build a `PATH` value with each `prepend` dir anchored at `root` placed
/// before `existing_path`, all joined with `:`.
pub fn prepended_path(prepend: &[String], root: &Path, existing_path: &str) -> String {
    if prepend.is_empty() {
        return existing_path.to_string();
    }
    let dirs: Vec<String> = prepend
        .iter()
        .map(|dir| root.join(dir).to_string_lossy().into_owned())
        .collect();
    if existing_path.is_empty() {
        return dirs.join(":");
    }
    format!("{}:{}", dirs.join(":"), existing_path)
}

/// Compute the environment overrides for a local discovery command:
/// the config env entries with `{root}` resolved against `root`, sorted by
/// key for determinism, plus a `PATH` entry prepending the root-anchored
/// `prepend_path` dirs to `existing_path` when `prepend_path` is non-empty.
/// Returns an empty vec when neither field is set, leaving the child
/// environment untouched.
pub fn discovery_env(
    env: &HashMap<String, String>,
    prepend_path: Option<&[String]>,
    root: &Path,
    existing_path: &str,
) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = resolve_env(env, root).into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(dirs) = prepend_path
        && !dirs.is_empty()
    {
        entries.push((
            "PATH".to_string(),
            prepended_path(dirs, root, existing_path),
        ));
    }
    entries
}

/// Apply framework env entries and `prepend_path` to a local discovery
/// command, resolving `{root}` against the local working directory.
pub(crate) fn apply_discovery_env(
    cmd: &mut tokio::process::Command,
    env: &HashMap<String, String>,
    prepend_path: Option<&[String]>,
) -> FrameworkResult<()> {
    let cwd = std::env::current_dir().map_err(|e| {
        FrameworkError::DiscoveryFailed(format!("Failed to get current directory: {}", e))
    })?;
    let existing_path = std::env::var("PATH").unwrap_or_default();
    for (key, value) in discovery_env(env, prepend_path, &cwd, &existing_path) {
        cmd.env(key, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_env_substitutes_root_placeholder() {
        let entries = HashMap::from([("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]);
        let resolved = resolve_env(&entries, Path::new("/app"));
        assert_eq!(
            resolved.get("VIRTUAL_ENV").map(String::as_str),
            Some("/app/.venv")
        );
    }

    #[test]
    fn test_resolve_env_substitutes_multiple_occurrences() {
        let entries = HashMap::from([(
            "FLAGS".to_string(),
            "--root={root} --data={root}/data".to_string(),
        )]);
        let resolved = resolve_env(&entries, Path::new("/app"));
        assert_eq!(
            resolved.get("FLAGS").map(String::as_str),
            Some("--root=/app --data=/app/data")
        );
    }

    #[test]
    fn test_resolve_env_leaves_values_without_placeholder() {
        let entries = HashMap::from([("PLAIN".to_string(), "value".to_string())]);
        let resolved = resolve_env(&entries, Path::new("/app"));
        assert_eq!(resolved.get("PLAIN").map(String::as_str), Some("value"));
    }

    #[test]
    fn test_resolve_env_empty_map() {
        let resolved = resolve_env(&HashMap::new(), Path::new("/app"));
        assert!(resolved.is_empty());
    }

    #[test]
    fn test_prepended_path_order() {
        let prepend = vec![".venv/bin".to_string(), "scripts".to_string()];
        let path = prepended_path(&prepend, Path::new("/app"), "/usr/bin:/bin");
        assert_eq!(path, "/app/.venv/bin:/app/scripts:/usr/bin:/bin");
    }

    #[test]
    fn test_prepended_path_empty_prepend_returns_existing() {
        let path = prepended_path(&[], Path::new("/app"), "/usr/bin");
        assert_eq!(path, "/usr/bin");
    }

    #[test]
    fn test_prepended_path_empty_existing() {
        let prepend = vec![".venv/bin".to_string()];
        let path = prepended_path(&prepend, Path::new("/app"), "");
        assert_eq!(path, "/app/.venv/bin");
    }

    #[test]
    fn test_prepended_path_absolute_entry_left_unchanged() {
        let prepend = vec!["/opt/tools/bin".to_string()];
        let path = prepended_path(&prepend, Path::new("/app"), "/usr/bin");
        assert_eq!(path, "/opt/tools/bin:/usr/bin");
    }

    #[test]
    fn test_discovery_env_resolves_root_against_cwd() {
        let env = HashMap::from([("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]);
        let entries = discovery_env(&env, None, Path::new("/repo"), "/usr/bin");
        assert_eq!(
            entries,
            vec![("VIRTUAL_ENV".to_string(), "/repo/.venv".to_string())]
        );
    }

    #[test]
    fn test_discovery_env_prepends_path_after_env_entries() {
        let env = HashMap::from([("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]);
        let prepend = vec![".venv/bin".to_string(), "scripts".to_string()];
        let entries = discovery_env(&env, Some(&prepend), Path::new("/repo"), "/usr/bin:/bin");
        assert_eq!(
            entries,
            vec![
                ("VIRTUAL_ENV".to_string(), "/repo/.venv".to_string()),
                (
                    "PATH".to_string(),
                    "/repo/.venv/bin:/repo/scripts:/usr/bin:/bin".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_discovery_env_sorts_entries_by_key() {
        let env = HashMap::from([
            ("ZULU".to_string(), "1".to_string()),
            ("ALPHA".to_string(), "2".to_string()),
        ]);
        let entries = discovery_env(&env, None, Path::new("/repo"), "/usr/bin");
        assert_eq!(
            entries,
            vec![
                ("ALPHA".to_string(), "2".to_string()),
                ("ZULU".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn test_discovery_env_prepend_only_emits_only_path() {
        let prepend = vec![".venv/bin".to_string()];
        let entries = discovery_env(
            &HashMap::new(),
            Some(&prepend),
            Path::new("/repo"),
            "/usr/bin",
        );
        assert_eq!(
            entries,
            vec![("PATH".to_string(), "/repo/.venv/bin:/usr/bin".to_string())]
        );
    }

    #[test]
    fn test_discovery_env_absent_fields_produce_no_overrides() {
        let entries = discovery_env(&HashMap::new(), None, Path::new("/repo"), "/usr/bin");
        assert!(entries.is_empty());

        let entries = discovery_env(&HashMap::new(), Some(&[]), Path::new("/repo"), "/usr/bin");
        assert!(entries.is_empty());
    }
}
