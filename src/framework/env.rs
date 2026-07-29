//! Resolution of framework-level `env` and `prepend_path` configuration.
//!
//! Framework commands run in two contexts: test discovery on the local
//! machine (root = local working directory) and test execution inside a
//! sandbox (root = `OFFLOAD_ROOT`, i.e. `sandbox_project_root`). The helpers
//! here take the applicable root so callers resolve config values for the
//! context they are about to run in.
//!
//! The two execution modes share the root-anchoring and `{root}`
//! substitution rules but differ in output shape: local providers need a
//! concrete merged `PATH` value for `process.env`, while remote-shell
//! providers render a `PATH=...` string with `$PATH` left for the remote
//! shell to expand. The string-rendering helpers and the value-computing
//! helpers both build on the same anchoring core.
//!
//! Precedence rule shared by both contexts: `prepend_path` dirs are placed
//! ahead of an explicit `env` `PATH` value when one is configured, and only
//! fall back to the ambient PATH (`$PATH` for remote shells, the process
//! PATH locally) when no explicit `env` `PATH` is set. Neither value is
//! dropped.

use std::collections::HashMap;
use std::path::Path;

use super::{FrameworkError, FrameworkResult};
use crate::provider::Command;

/// Substitute the `{root}` placeholder in a single env value against a
/// root anchor string.
pub fn resolve_root_placeholder(value: &str, root: &str) -> String {
    value.replace("{root}", root)
}

/// Substitute the `{root}` placeholder in each env value.
pub fn resolve_env(entries: &HashMap<String, String>, root: &Path) -> HashMap<String, String> {
    let root = root.to_string_lossy();
    entries
        .iter()
        .map(|(key, value)| (key.clone(), resolve_root_placeholder(value, root.as_ref())))
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

/// Anchor each `prepend` dir at `root`, preserving order. Absolute dirs
/// pass through `Path::join` unchanged.
fn anchored_dirs(prepend: &[String], root: &Path) -> Vec<String> {
    prepend
        .iter()
        .map(|dir| root.join(dir).to_string_lossy().into_owned())
        .collect()
}

/// Build a `PATH` value with each `prepend` dir anchored at `root` placed
/// before `existing_path`, all joined with `:`.
pub fn prepended_path(prepend: &[String], root: &Path, existing_path: &str) -> String {
    if prepend.is_empty() {
        return existing_path.to_string();
    }
    let dirs = anchored_dirs(prepend, root);
    if existing_path.is_empty() {
        return dirs.join(":");
    }
    format!("{}:{}", dirs.join(":"), existing_path)
}

/// Render the `PATH=...` env entry for a remote shell command: the
/// root-anchored `prepend` dirs shell-quoted and colon-joined, followed by
/// the base PATH. The base is the explicit env `PATH` value
/// (`explicit_path`, already `{root}`-resolved, shell-quoted here) when one
/// is configured; otherwise an unquoted `:"$PATH"` suffix left for the
/// remote shell to expand. Either way the rendered command carries a
/// single `PATH` entry, so neither value is dropped or clobbered. Callers
/// skip rendering entirely when `prepend` is empty.
pub fn shell_path_prepend_entry(
    prepend: &[String],
    root: &str,
    explicit_path: Option<&str>,
) -> String {
    let joined = anchored_dirs(prepend, Path::new(root)).join(":");
    let dirs = shell_words::quote(&joined);
    match explicit_path {
        Some(path) => format!("PATH={}:{}", dirs, shell_words::quote(path)),
        None => format!("PATH={}:\"$PATH\"", dirs),
    }
}

/// Compute the environment overrides for a local discovery command:
/// the config env entries with `{root}` resolved against `root`, sorted by
/// key for determinism, plus the root-anchored `prepend_path` dirs
/// prepended to `PATH` when `prepend_path` is non-empty. Precedence: an
/// explicit env `PATH` entry is the base for the prepend (merged in place,
/// never duplicated or dropped); `existing_path` is only the fallback when
/// no explicit env `PATH` is configured. Returns an empty vec when neither
/// field is set, leaving the child environment untouched.
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
        let base = entries
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_str())
            .unwrap_or(existing_path);
        let merged = prepended_path(dirs, root, base);
        match entries.iter_mut().find(|(key, _)| key == "PATH") {
            Some(entry) => entry.1 = merged,
            None => entries.push(("PATH".to_string(), merged)),
        }
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
    fn test_resolve_root_placeholder_substitutes_all_occurrences() {
        assert_eq!(
            resolve_root_placeholder("--root={root} --data={root}/data", "/app"),
            "--root=/app --data=/app/data"
        );
    }

    #[test]
    fn test_resolve_root_placeholder_leaves_values_without_placeholder() {
        assert_eq!(resolve_root_placeholder("value", "/app"), "value");
    }

    #[test]
    fn test_shell_path_prepend_entry_quotes_dirs_and_defers_path() {
        let prepend = vec![".venv/bin".to_string(), "scripts".to_string()];
        assert_eq!(
            shell_path_prepend_entry(&prepend, "/app", None),
            "PATH=/app/.venv/bin:/app/scripts:\"$PATH\""
        );
    }

    #[test]
    fn test_shell_path_prepend_entry_quotes_dirs_with_spaces() {
        let prepend = vec!["my tools/bin".to_string()];
        assert_eq!(
            shell_path_prepend_entry(&prepend, "/app", None),
            "PATH='/app/my tools/bin':\"$PATH\""
        );
    }

    #[test]
    fn test_shell_path_prepend_entry_absolute_entry_left_unchanged() {
        let prepend = vec!["/opt/tools/bin".to_string()];
        assert_eq!(
            shell_path_prepend_entry(&prepend, "/app", None),
            "PATH=/opt/tools/bin:\"$PATH\""
        );
    }

    #[test]
    fn test_shell_path_prepend_entry_prepends_onto_explicit_path() {
        let prepend = vec![".venv/bin".to_string()];
        assert_eq!(
            shell_path_prepend_entry(&prepend, "/app", Some("/custom/bin")),
            "PATH=/app/.venv/bin:/custom/bin"
        );
    }

    #[test]
    fn test_shell_path_prepend_entry_quotes_explicit_path_with_spaces() {
        let prepend = vec![".venv/bin".to_string()];
        assert_eq!(
            shell_path_prepend_entry(&prepend, "/app", Some("/my tools/bin")),
            "PATH=/app/.venv/bin:'/my tools/bin'"
        );
    }

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

    #[test]
    fn test_discovery_env_prepends_onto_explicit_env_path() {
        let env = HashMap::from([("PATH".to_string(), "{root}/custom/bin".to_string())]);
        let prepend = vec![".venv/bin".to_string()];
        let entries = discovery_env(&env, Some(&prepend), Path::new("/repo"), "/usr/bin");
        assert_eq!(
            entries,
            vec![(
                "PATH".to_string(),
                "/repo/.venv/bin:/repo/custom/bin".to_string(),
            )]
        );
    }

    #[test]
    fn test_discovery_env_explicit_path_alone_passes_through() {
        let env = HashMap::from([("PATH".to_string(), "/custom/bin".to_string())]);
        let entries = discovery_env(&env, None, Path::new("/repo"), "/usr/bin");
        assert_eq!(
            entries,
            vec![("PATH".to_string(), "/custom/bin".to_string())]
        );
    }
}
