//! Why each note entry on a commit does or does not serve the current recipe.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use super::recipe::Recipe;
use crate::git;

/// Why a note entry does or does not serve the current recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The stored recipe equals the current one and the entry carries an image.
    Matches,
    /// The dotted JSON paths at which the stored recipe differs from the current one, plus
    /// `image_id` when the entry carries no image.
    Differs(Vec<String>),
    /// No stored recipe: written by an older Offload and never served.
    Legacy,
}

/// One note entry classified against the current recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EntryVerdict {
    name: String,
    image_id: String,
    pub(super) verdict: Verdict,
}

/// Read the note on `commit_sha` and classify its entries against `recipe`; `None` when the
/// commit carries no note.
pub(super) async fn classify_note(
    repo: &Path,
    commit_sha: &str,
    recipe: &Recipe,
) -> Result<Option<Vec<EntryVerdict>>> {
    let current = serde_json::to_value(recipe).context("failed to serialize recipe")?;
    let note = git::read_note(repo, commit_sha)
        .await
        .context("failed to read git note for checkpoint commit")?;
    Ok(note.map(|contents| classify_entries(&contents, &current)))
}

/// Classify every entry of a note against the current recipe, ordered by entry name so the
/// lookup and `offload checkpoint-status` agree on which match comes first.
fn classify_entries(contents: &git::NoteContents, current: &Value) -> Vec<EntryVerdict> {
    contents
        .iter()
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .map(|(name, entry)| EntryVerdict {
            name: name.clone(),
            image_id: entry.image_id.clone(),
            verdict: classify_entry(entry, current),
        })
        .collect()
}

/// An entry matches only when its stored recipe equals `current` and it carries an image.
fn classify_entry(entry: &git::ImageEntry, current: &Value) -> Verdict {
    let Some(stored) = entry.recipe.as_ref() else {
        return Verdict::Legacy;
    };
    let mut paths = differing_paths(stored, current);
    if entry.image_id.is_empty() {
        paths.push("image_id".to_string());
    }
    if paths.is_empty() {
        Verdict::Matches
    } else {
        Verdict::Differs(paths)
    }
}

/// The image served for the current recipe: the first entry that matches.
pub(super) fn matching_image(verdicts: &[EntryVerdict]) -> Option<&str> {
    verdicts
        .iter()
        .find(|entry| entry.verdict == Verdict::Matches)
        .map(|entry| entry.image_id.as_str())
}

/// Dotted JSON paths at which `stored` differs from `current`.  Objects are descended, a key on
/// one side only is a difference at that key, and arrays and scalars are compared whole; a
/// stored value that is not an object at all is reported as `recipe`.
fn differing_paths(stored: &Value, current: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_differing_paths(stored, current, None, &mut paths);
    paths
}

/// Recursive worker for [`differing_paths`]; `path` is the dotted prefix of the values being
/// compared, `None` at the root.
fn collect_differing_paths(
    stored: &Value,
    current: &Value,
    path: Option<&str>,
    paths: &mut Vec<String>,
) {
    match (stored, current) {
        (Value::Object(stored_fields), Value::Object(current_fields)) => {
            let keys: BTreeSet<&String> =
                stored_fields.keys().chain(current_fields.keys()).collect();
            for key in keys {
                let child = match path {
                    Some(prefix) => format!("{prefix}.{key}"),
                    None => key.clone(),
                };
                match (stored_fields.get(key), current_fields.get(key)) {
                    (Some(a), Some(b)) => collect_differing_paths(a, b, Some(&child), paths),
                    _ => paths.push(child),
                }
            }
        }
        _ if stored != current => paths.push(path.unwrap_or("recipe").to_string()),
        _ => {}
    }
}

/// The `offload checkpoint-status` line for one entry.
pub(super) fn entry_line(entry: &EntryVerdict) -> String {
    let state = match &entry.verdict {
        Verdict::Matches => "matches".to_string(),
        Verdict::Differs(paths) => format!("differs: {}", paths.join(", ")),
        Verdict::Legacy => "legacy (written by an older Offload)".to_string(),
    };
    format!("  {} {} {}", entry.name, entry.image_id, state)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::git::{ImageEntry, NoteContents};
    use crate::image_cache::test_support::{minimal_modal_config, recipe_for};

    /// Helper: the verdict for one note entry.
    fn entry_verdict(name: &str, image_id: &str, verdict: Verdict) -> EntryVerdict {
        EntryVerdict {
            name: name.to_string(),
            image_id: image_id.to_string(),
            verdict,
        }
    }

    #[test]
    fn test_differing_paths_reports_top_level_fields_whole() -> Result<()> {
        let stored = json!({"format": 1, "copy_dirs": [], "sandbox_init_cmd": "bash old.sh"});
        let current = json!({
            "format": 1,
            "copy_dirs": [{"source": "data", "target": "/data", "content": null}],
            "sandbox_init_cmd": "bash new.sh"
        });
        assert_eq!(
            differing_paths(&stored, &current),
            vec!["copy_dirs", "sandbox_init_cmd"]
        );
        Ok(())
    }

    #[test]
    fn test_differing_paths_descends_into_the_builder() -> Result<()> {
        let stored = json!({"builder": {"modal": {"script": "9b1d", "dockerfile": "Dockerfile"}}});
        let current =
            json!({"builder": {"modal": {"script": "9b1d", "dockerfile": "other/Dockerfile"}}});
        assert_eq!(
            differing_paths(&stored, &current),
            vec!["builder.modal.dockerfile"]
        );
        Ok(())
    }

    #[test]
    fn test_differing_paths_reports_a_key_on_one_side_only() -> Result<()> {
        let stored = json!({"format": 1, "newer_field": true});
        let current = json!({"format": 1, "export_extras": null});
        assert_eq!(
            differing_paths(&stored, &current),
            vec!["export_extras", "newer_field"]
        );
        Ok(())
    }

    #[test]
    fn test_differing_paths_is_empty_for_equal_values() -> Result<()> {
        let recipe = serde_json::to_value(recipe_for(&minimal_modal_config())?)?;
        assert!(differing_paths(&recipe, &recipe).is_empty());
        Ok(())
    }

    #[test]
    fn test_classify_entries_explains_every_entry() -> Result<()> {
        let current = serde_json::to_value(recipe_for(&minimal_modal_config())?)?;
        let mut changed = minimal_modal_config();
        changed.offload.sandbox_init_cmd = Some("bash other.sh".to_string());
        let other = serde_json::to_value(recipe_for(&changed)?)?;

        let mut contents = NoteContents::new();
        contents.insert(
            "recipe:aaaa".to_string(),
            ImageEntry {
                recipe: Some(current.clone()),
                ..ImageEntry::legacy("im-hit")
            },
        );
        contents.insert(
            "recipe:bbbb".to_string(),
            ImageEntry {
                recipe: Some(other),
                ..ImageEntry::legacy("im-other")
            },
        );
        contents.insert(
            "recipe:cccc".to_string(),
            ImageEntry {
                recipe: Some(current.clone()),
                ..ImageEntry::legacy("")
            },
        );
        contents.insert("offload.toml".to_string(), ImageEntry::legacy("im-legacy"));

        assert_eq!(
            classify_entries(&contents, &current),
            vec![
                entry_verdict("offload.toml", "im-legacy", Verdict::Legacy),
                entry_verdict("recipe:aaaa", "im-hit", Verdict::Matches),
                entry_verdict(
                    "recipe:bbbb",
                    "im-other",
                    Verdict::Differs(vec!["sandbox_init_cmd".to_string()])
                ),
                entry_verdict(
                    "recipe:cccc",
                    "",
                    Verdict::Differs(vec!["image_id".to_string()])
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_entry_line_states_each_verdict() -> Result<()> {
        assert_eq!(
            entry_line(&entry_verdict(
                "recipe:3f9c2a7b1e5d4c08",
                "im-abc",
                Verdict::Matches
            )),
            "  recipe:3f9c2a7b1e5d4c08 im-abc matches"
        );
        assert_eq!(
            entry_line(&entry_verdict(
                "recipe:9c0d1e2f3a4b5c6d",
                "im-def",
                Verdict::Differs(vec![
                    "builder.modal.dockerfile".to_string(),
                    "sandbox_init_cmd".to_string()
                ])
            )),
            "  recipe:9c0d1e2f3a4b5c6d im-def differs: builder.modal.dockerfile, sandbox_init_cmd"
        );
        assert_eq!(
            entry_line(&entry_verdict(
                "offload-modal.toml",
                "im-ghi",
                Verdict::Legacy
            )),
            "  offload-modal.toml im-ghi legacy (written by an older Offload)"
        );
        Ok(())
    }
}
