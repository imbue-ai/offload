//! Resolution of framework-level `env` and `prepend_path` configuration.
//!
//! Framework commands run in two contexts: test discovery on the local
//! machine (root = local working directory) and test execution inside a
//! sandbox (root = `OFFLOAD_ROOT`, i.e. `sandbox_project_root`). The helpers
//! here take the applicable root so callers resolve config values for the
//! context they are about to run in.

use std::collections::HashMap;
use std::path::Path;

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
}
