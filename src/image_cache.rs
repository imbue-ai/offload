//! Image cache resolution logic for sandbox image caching via git notes.
//!
//! This module resolves checkpoint commits and their cached images by reading
//! git notes. It does NOT build images or call providers -- it only reads
//! existing cached data. Building and caching (writing notes, pushing) is
//! done by the caller.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::schema::CheckpointConfig;
use crate::git;

/// A cached image entry found in a git note for a checkpoint commit.
#[derive(Debug, Clone)]
pub struct CachedImage {
    /// The cached image identifier.
    pub image_id: String,
}

/// Information about a resolved checkpoint commit.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    /// The SHA of the checkpoint commit.
    pub checkpoint_sha: String,
    /// The cached image for this checkpoint, if one exists in git notes.
    pub cached_image: Option<CachedImage>,
}

/// Information about a resolved latest-commit base.
#[derive(Debug, Clone)]
pub struct LatestCommitInfo {
    /// The SHA of the latest commit (HEAD).
    pub head_sha: String,
    /// The cached image for this commit, if one exists in git notes.
    pub cached_image: Option<CachedImage>,
}

/// In non-checkpoint mode: resolve the latest commit (HEAD) and its cached image (if any).
///
/// Returns `None` if there are no commits (empty repo).
/// Returns `Some(LatestCommitInfo { cached_image: None })` if HEAD exists
/// but has no cached image in git notes.
pub async fn resolve_latest_commit(
    repo: &Path,
    config_path: &str,
) -> Result<Option<LatestCommitInfo>> {
    let head_sha = match git::head_sha(repo).await {
        Ok(sha) => sha,
        Err(_) => return Ok(None),
    };

    let repo_root = git::repo_root(repo).await?;
    let config_key = git::canonicalize_config_path(config_path, &repo_root)?;

    let cached_image = read_cached_image_for_commit(repo, &head_sha, &config_key).await?;

    Ok(Some(LatestCommitInfo {
        head_sha,
        cached_image,
    }))
}

/// Find the nearest checkpoint ancestor SHA without reading git notes.
///
/// Walks up to `max_depth` ancestors of HEAD looking for the first commit
/// that touches any of the configured `build_inputs` paths.
///
/// Returns `None` if no checkpoint commit is found within the ancestor window.
pub async fn find_checkpoint_sha(
    repo: &Path,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<String>> {
    let ancestors = git::ancestors(repo, max_depth).await?;

    for sha in &ancestors {
        let touches = git::commit_touches_paths(repo, sha, &checkpoint_cfg.build_inputs).await?;
        if touches {
            return Ok(Some(sha.clone()));
        }
    }

    Ok(None)
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
    repo: &Path,
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<CheckpointInfo>> {
    let repo_root = git::repo_root(repo).await?;
    let config_key = git::canonicalize_config_path(config_path, &repo_root)?;
    let ancestors = git::ancestors(repo, max_depth).await?;

    for sha in &ancestors {
        let touches = git::commit_touches_paths(repo, sha, &checkpoint_cfg.build_inputs).await?;
        if !touches {
            continue;
        }

        // Found a checkpoint commit -- check for cached image
        let cached_image = read_cached_image_for_commit(repo, sha, &config_key).await?;
        return Ok(Some(CheckpointInfo {
            checkpoint_sha: sha.clone(),
            cached_image,
        }));
    }

    Ok(None)
}

/// Read the cached image entry from a git note for a specific commit and config key.
async fn read_cached_image_for_commit(
    repo: &Path,
    commit_sha: &str,
    config_key: &str,
) -> Result<Option<CachedImage>> {
    let note = git::read_note(repo, commit_sha)
        .await
        .context("failed to read git note for checkpoint commit")?;

    let Some(contents) = note else {
        return Ok(None);
    };

    match contents.get(config_key) {
        Some(entry) if !entry.image_id.is_empty() => Ok(Some(CachedImage {
            image_id: entry.image_id.clone(),
        })),
        _ => Ok(None),
    }
}

/// How the base commit was determined.
pub enum BaseKind {
    /// Nearest ancestor touching `build_inputs` (from `[checkpoint]` config).
    Checkpoint,
    /// Latest commit (HEAD).
    LatestCommit,
}

impl BaseKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Checkpoint => "Checkpoint",
            Self::LatestCommit => "Latest-commit",
        }
    }
}

/// Pre-resolved base commit and its cached image, determined before provider dispatch.
pub struct ResolvedBase {
    pub base_sha: String,
    pub cached_image_id: Option<String>,
    pub kind: BaseKind,
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

        let result = resolve_checkpoint(dir.path(), "offload.toml", &cfg, 10).await?;

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

        let result = resolve_checkpoint(dir.path(), "offload.toml", &cfg, 10).await?;

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

        let result = resolve_checkpoint(dir.path(), "offload.toml", &cfg, 10).await?;

        let info = result.context("should find checkpoint")?;
        assert_eq!(info.checkpoint_sha, checkpoint_sha);
        let cached = info.cached_image.context("should have cached image")?;
        assert_eq!(cached.image_id, "im-cached123");
        Ok(())
    }

    // ---- Tests for resolve_latest_commit ----

    #[tokio::test]
    async fn test_resolve_latest_commit_hit() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;

        // Write a note on HEAD
        let mut contents = NoteContents::new();
        contents.insert(
            "offload.toml".to_string(),
            ImageEntry {
                image_id: "im-head-cached".to_string(),
            },
        );
        write_note_in(dir.path(), &head, &contents)?;

        let result = resolve_latest_commit(dir.path(), "offload.toml").await?;

        let info = result.context("should find latest commit")?;
        assert_eq!(info.head_sha, head);
        let cached = info.cached_image.context("should have cached image")?;
        assert_eq!(cached.image_id, "im-head-cached");
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_latest_commit_miss() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;

        // No note on HEAD
        let result = resolve_latest_commit(dir.path(), "offload.toml").await?;

        let info = result.context("should find latest commit (miss still returns info)")?;
        assert_eq!(info.head_sha, head);
        assert!(
            info.cached_image.is_none(),
            "no note on HEAD means no cached image"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_latest_commit_empty_repo() -> Result<()> {
        let dir = tempfile::tempdir()?;
        git_cmd(dir.path(), &["init"])?;
        // Empty repo — no commits

        let result = resolve_latest_commit(dir.path(), "offload.toml").await?;

        assert!(result.is_none(), "empty repo has no HEAD");
        Ok(())
    }
}
