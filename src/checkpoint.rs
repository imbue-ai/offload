//! Checkpoint resolution logic for sandbox image caching via git notes.
//!
//! This module resolves checkpoint commits and their cached images by reading
//! git notes. It does NOT build images or call providers -- it only reads
//! existing cached data. Building and caching (writing notes, pushing) is
//! done by the caller.

use anyhow::{Context, Result};

use crate::config::schema::CheckpointConfig;
use crate::git;

/// A cached image entry found in a git note for a checkpoint commit.
#[derive(Debug, Clone)]
pub struct CachedImage {
    /// The cached image identifier.
    pub image_id: String,
    /// The build inputs hash stored alongside the image, if any.
    pub build_inputs_hash: Option<String>,
}

/// Information about a resolved checkpoint commit.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    /// The SHA of the checkpoint commit.
    pub checkpoint_sha: String,
    /// The cached image for this checkpoint, if one exists in git notes.
    pub cached_image: Option<CachedImage>,
}

/// In non-checkpoint mode: look up the parent commit's cached image.
///
/// Returns `Some((parent_sha, image_id))` if the parent commit (HEAD~1)
/// has a cached image in git notes for the given config path.
/// Returns `None` if this is the initial commit (no parent) or the parent
/// has no cached image.
pub async fn resolve_parent_base(config_path: &str) -> Result<Option<(String, String)>> {
    let parent = git::parent_sha().await?;
    let Some(parent_sha) = parent else {
        return Ok(None);
    };

    let repo_root = git::repo_root().await?;
    let config_key = git::canonicalize_config_path(config_path, &repo_root)?;

    let note = git::read_note(&parent_sha).await?;
    let Some(contents) = note else {
        return Ok(None);
    };

    match contents.get(&config_key) {
        Some(entry) if !entry.image_id.is_empty() => Ok(Some((parent_sha, entry.image_id.clone()))),
        _ => Ok(None),
    }
}

/// Find the nearest checkpoint ancestor and its cached image information.
///
/// Walks up to `max_depth` ancestors of HEAD looking for the first commit
/// that touches any of the configured `build_inputs` paths. If found, reads
/// the git note for that commit to check for a cached image.
///
/// Returns `None` if no checkpoint commit is found within the ancestor window.
/// Returns `Some(CheckpointInfo { cached_image: None })` if a checkpoint commit
/// is found but has no cached image in git notes.
pub async fn resolve_checkpoint(
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<CheckpointInfo>> {
    let repo_root = git::repo_root().await?;
    let config_key = git::canonicalize_config_path(config_path, &repo_root)?;
    let ancestors = git::ancestors(max_depth).await?;

    for sha in &ancestors {
        let touches = git::commit_touches_paths(sha, &checkpoint_cfg.build_inputs).await?;
        if !touches {
            continue;
        }

        // Found a checkpoint commit -- check for cached image
        let cached_image = read_cached_image_for_commit(sha, &config_key).await?;
        return Ok(Some(CheckpointInfo {
            checkpoint_sha: sha.clone(),
            cached_image,
        }));
    }

    Ok(None)
}

/// Read the cached image entry from a git note for a specific commit and config key.
async fn read_cached_image_for_commit(
    commit_sha: &str,
    config_key: &str,
) -> Result<Option<CachedImage>> {
    let note = git::read_note(commit_sha)
        .await
        .context("failed to read git note for checkpoint commit")?;

    let Some(contents) = note else {
        return Ok(None);
    };

    match contents.get(config_key) {
        Some(entry) if !entry.image_id.is_empty() => Ok(Some(CachedImage {
            image_id: entry.image_id.clone(),
            build_inputs_hash: entry.build_inputs_hash.clone(),
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{ImageEntry, NOTES_REF, NoteContents};

    use std::path::Path;

    use anyhow::bail;

    /// Helper: create a temp directory with an initialized git repo and one commit.
    fn init_temp_repo() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        git_cmd(dir.path(), &["init"])?;
        git_cmd(dir.path(), &["config", "user.email", "test@test.com"])?;
        git_cmd(dir.path(), &["config", "user.name", "Test"])?;
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# test repo")?;
        git_cmd(dir.path(), &["add", "README.md"])?;
        git_cmd(dir.path(), &["commit", "-m", "initial commit"])?;
        Ok(dir)
    }

    /// Helper: run a git command in a directory and return stdout.
    fn git_cmd(dir: &Path, args: &[&str]) -> Result<String> {
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
    fn write_note_in(dir: &Path, sha: &str, contents: &NoteContents) -> Result<()> {
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
    fn read_note_in(dir: &Path, sha: &str) -> Result<Option<NoteContents>> {
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
    fn head_sha_in(dir: &Path) -> Result<String> {
        git_cmd(dir, &["rev-parse", "HEAD"])
    }

    // ---- Tests for resolve_checkpoint ----

    // These tests use set_current_dir to point at temp repos, since the
    // production git functions operate on cwd. Each test saves and restores
    // cwd around the async calls.

    /// RAII guard that restores the working directory when dropped.
    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set(dir: &Path) -> Result<Self> {
            let original = std::env::current_dir()?;
            std::env::set_current_dir(dir)?;
            Ok(Self { original })
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[tokio::test]
    async fn test_resolve_checkpoint_finds_nearest() -> Result<()> {
        let dir = init_temp_repo()?;

        // Commit 1 (initial) already exists from init_temp_repo
        // Commit 2: touches Dockerfile (this is our checkpoint)
        std::fs::write(dir.path().join("Dockerfile"), "FROM ubuntu:22.04")?;
        git_cmd(dir.path(), &["add", "Dockerfile"])?;
        git_cmd(dir.path(), &["commit", "-m", "add Dockerfile"])?;
        let checkpoint_sha = head_sha_in(dir.path())?;

        // Commit 3: does NOT touch Dockerfile
        std::fs::write(dir.path().join("app.py"), "print('hello')")?;
        git_cmd(dir.path(), &["add", "app.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add app"])?;

        // Commit 4: does NOT touch Dockerfile
        std::fs::write(dir.path().join("test.py"), "assert True")?;
        git_cmd(dir.path(), &["add", "test.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add test"])?;

        let cfg = CheckpointConfig {
            build_inputs: vec!["Dockerfile".to_string()],
        };

        let _guard = CwdGuard::set(dir.path())?;
        let result = resolve_checkpoint("offload.toml", &cfg, 10).await?;

        let info = result.context("should find checkpoint")?;
        assert_eq!(info.checkpoint_sha, checkpoint_sha);
        assert!(info.cached_image.is_none(), "no note written, no cache");
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_checkpoint_none_when_no_match() -> Result<()> {
        let dir = init_temp_repo()?;

        // Only the initial commit exists with README.md.
        // Add another commit that doesn't touch Dockerfile.
        std::fs::write(dir.path().join("app.py"), "print('hello')")?;
        git_cmd(dir.path(), &["add", "app.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add app"])?;

        let cfg = CheckpointConfig {
            build_inputs: vec!["Dockerfile".to_string()],
        };

        let _guard = CwdGuard::set(dir.path())?;
        let result = resolve_checkpoint("offload.toml", &cfg, 10).await?;

        assert!(result.is_none(), "no commit touches Dockerfile");
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_checkpoint_with_cached_image() -> Result<()> {
        let dir = init_temp_repo()?;

        // Create a checkpoint commit that touches Dockerfile
        std::fs::write(dir.path().join("Dockerfile"), "FROM ubuntu:22.04")?;
        git_cmd(dir.path(), &["add", "Dockerfile"])?;
        git_cmd(dir.path(), &["commit", "-m", "add Dockerfile"])?;
        let checkpoint_sha = head_sha_in(dir.path())?;

        // Write a note on the checkpoint commit
        let mut contents = NoteContents::new();
        contents.insert(
            "offload.toml".to_string(),
            ImageEntry {
                image_id: "im-cached123".to_string(),
                build_inputs_hash: Some("abc123hash".to_string()),
            },
        );
        write_note_in(dir.path(), &checkpoint_sha, &contents)?;

        // Add another commit that doesn't touch Dockerfile
        std::fs::write(dir.path().join("app.py"), "print('hello')")?;
        git_cmd(dir.path(), &["add", "app.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add app"])?;

        let cfg = CheckpointConfig {
            build_inputs: vec!["Dockerfile".to_string()],
        };

        let _guard = CwdGuard::set(dir.path())?;
        let result = resolve_checkpoint("offload.toml", &cfg, 10).await?;

        let info = result.context("should find checkpoint")?;
        assert_eq!(info.checkpoint_sha, checkpoint_sha);
        let cached = info.cached_image.context("should have cached image")?;
        assert_eq!(cached.image_id, "im-cached123");
        assert_eq!(cached.build_inputs_hash.as_deref(), Some("abc123hash"));
        Ok(())
    }

    // ---- Tests for resolve_parent_base ----

    #[tokio::test]
    async fn test_resolve_parent_base_hit() -> Result<()> {
        let dir = init_temp_repo()?;
        let initial_sha = head_sha_in(dir.path())?;

        // Write a note on the initial commit
        let mut contents = NoteContents::new();
        contents.insert(
            "offload.toml".to_string(),
            ImageEntry {
                image_id: "im-parent-cached".to_string(),
                build_inputs_hash: None,
            },
        );
        write_note_in(dir.path(), &initial_sha, &contents)?;

        // Create a second commit so HEAD~1 = initial commit
        std::fs::write(dir.path().join("app.py"), "print('hello')")?;
        git_cmd(dir.path(), &["add", "app.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add app"])?;

        let _guard = CwdGuard::set(dir.path())?;
        let result = resolve_parent_base("offload.toml").await?;

        let (parent_sha, image_id) = result.context("should find parent base")?;
        assert_eq!(parent_sha, initial_sha);
        assert_eq!(image_id, "im-parent-cached");
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_parent_base_miss() -> Result<()> {
        let dir = init_temp_repo()?;

        // Create a second commit (no note on initial commit)
        std::fs::write(dir.path().join("app.py"), "print('hello')")?;
        git_cmd(dir.path(), &["add", "app.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add app"])?;

        let _guard = CwdGuard::set(dir.path())?;
        let result = resolve_parent_base("offload.toml").await?;

        assert!(result.is_none(), "no note on parent means no cached base");
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_parent_base_initial_commit() -> Result<()> {
        let dir = init_temp_repo()?;
        // Only one commit — no parent

        let _guard = CwdGuard::set(dir.path())?;
        let result = resolve_parent_base("offload.toml").await?;

        assert!(result.is_none(), "initial commit has no parent");
        Ok(())
    }
}
