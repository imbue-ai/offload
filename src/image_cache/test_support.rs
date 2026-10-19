//! Temp-repository and config fixtures shared by the image cache test modules.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::BuildContext;
use super::recipe::Recipe;
use crate::config::{
    Config, DefaultFrameworkConfig, FrameworkConfig, ModalProviderConfig, OffloadConfig,
    ProviderConfig,
};
use crate::git::{ImageEntry, NOTES_REF, NoteContents};

/// Helper: create a temp directory with an initialized git repo and one commit.
pub(super) fn init_temp_repo() -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    init_repo_in(dir.path())?;
    Ok(dir)
}

/// Helper: initialize a git repo with one commit in an existing directory.
pub(super) fn init_repo_in(dir: &Path) -> Result<()> {
    git_cmd(dir, &["init"])?;
    git_cmd(dir, &["config", "user.email", "test@test.com"])?;
    git_cmd(dir, &["config", "user.name", "Test"])?;
    let readme = dir.join("README.md");
    std::fs::write(&readme, "# test repo")?;
    git_cmd(dir, &["add", "README.md"])?;
    git_cmd(dir, &["commit", "-m", "initial commit"])?;
    Ok(())
}

/// Helper: run a git command in a directory and return stdout.
pub(super) fn git_cmd(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Helper: write a git note in a specific directory.
pub(super) fn write_note_in(dir: &Path, sha: &str, contents: &NoteContents) -> Result<()> {
    // Read existing note to merge
    let existing = read_note_in(dir, sha)?;
    let mut merged = existing.unwrap_or_default();
    for (key, value) in contents {
        merged.insert(key.clone(), value.clone());
    }
    let json = serde_json::to_string_pretty(&merged)?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), json.as_bytes())?;

    git_cmd(
        dir,
        &[
            "notes",
            "--ref",
            NOTES_REF,
            "add",
            "-f",
            "-F",
            &tmp.path().to_string_lossy(),
            sha,
        ],
    )?;
    Ok(())
}

/// Helper: read a git note in a specific directory.
pub(super) fn read_note_in(dir: &Path, sha: &str) -> Result<Option<NoteContents>> {
    let output = std::process::Command::new("git")
        .args(["notes", "--ref", NOTES_REF, "show", sha])
        .current_dir(dir)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_lowercase();
        if stderr_lower.contains("no note found") || stderr_lower.contains("not a valid ref") {
            return Ok(None);
        }
        bail!("git notes show failed: {}", stderr.trim());
    }
    let json = String::from_utf8(output.stdout)?;
    let contents: NoteContents = serde_json::from_str(&json)?;
    Ok(Some(contents))
}

/// Helper: get HEAD SHA in a directory.
pub(super) fn head_sha_in(dir: &Path) -> Result<String> {
    git_cmd(dir, &["rev-parse", "HEAD"])
}

/// Helper: a minimal Modal config; tests change the one field they vary.
pub(super) fn minimal_modal_config() -> Config {
    Config {
        offload: OffloadConfig {
            max_parallel: 1,
            test_timeout_secs: 60,
            working_dir: None,
            sandbox_project_root: Some("/app".to_string()),
            sandbox_repo_root: Some("/app".to_string()),
            sandbox_init_cmd: Some("bash init.sh".to_string()),
            post_patch_cmd: None,
            impatiently_requeue_batches: true,
            max_parallel_collection: 0,
        },
        provider: ProviderConfig::Modal(ModalProviderConfig {
            dockerfile: Some("Dockerfile".to_string()),
            ..ModalProviderConfig::default()
        }),
        framework: FrameworkConfig::Default(DefaultFrameworkConfig {
            discover_command: "echo test".to_string(),
            run_command: "run {tests}".to_string(),
            result_file: None,
            working_dir: None,
            test_id_format: "{name}".to_string(),
        }),
        groups: Default::default(),
        report: Default::default(),
        checkpoint: None,
        history: None,
    }
}

/// Helper: the recipe `offload build` derives from `config` at the repository root (no runtime
/// copy dirs).
pub(super) fn recipe_for(config: &Config) -> Result<Recipe> {
    Recipe::from_config(config, Path::new(""), &[])
        .context("the Modal provider should yield a recipe")
}

/// Helper: write an entry for `recipe` on `sha` the way a current Offload does.
pub(super) fn write_recipe_entry_in(
    dir: &Path,
    sha: &str,
    recipe: &Recipe,
    image_id: &str,
) -> Result<()> {
    let entry = ImageEntry {
        recipe: Some(serde_json::to_value(recipe)?),
        ..ImageEntry::legacy(image_id)
    };
    let mut contents = NoteContents::new();
    contents.insert(recipe.name()?, entry);
    write_note_in(dir, sha, &contents)
}

/// A context exported to `/ctx` from a repository at `/repo` invoked from its `sub/`, whose
/// relative copy sources are written against the invoking directory.
pub(super) fn build_context() -> BuildContext<'static> {
    build_context_from(Path::new(""))
}

/// [`build_context`] for a provider whose relative copy sources are written against `origin`.
pub(super) fn build_context_from(origin: &'static Path) -> BuildContext<'static> {
    BuildContext {
        dir: Path::new("/ctx"),
        cwd_rel: Path::new("sub"),
        root: Path::new("/repo"),
        origin,
    }
}
