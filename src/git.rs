//! Git operations for checkpoint image caching via git notes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Git notes ref used to store checkpoint image metadata.
pub const NOTES_REF: &str = "refs/notes/offload-images";

/// A cached image entry stored in a git note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    pub image_id: String,
}

/// A note is a JSON object keyed by TOML config file path.
pub type NoteContents = HashMap<String, ImageEntry>;

/// Run a git command and return its stdout as a trimmed string.
async fn run_git(args: &[&str]) -> Result<String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args(&args)
            .output()
            .context("failed to run git")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git {} failed (exit {}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            );
        }
        let stdout = String::from_utf8(output.stdout).context("git output was not valid UTF-8")?;
        Ok(stdout.trim().to_string())
    })
    .await?
}

/// Run a git command in a specific directory.
#[cfg(test)]
async fn run_git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let dir = dir.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .output()
            .context("failed to run git")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "git {} failed (exit {}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            );
        }
        let stdout = String::from_utf8(output.stdout).context("git output was not valid UTF-8")?;
        Ok(stdout.trim().to_string())
    })
    .await?
}

/// Return the SHA of HEAD.
pub async fn head_sha() -> Result<String> {
    run_git(&["rev-parse", "HEAD"]).await
}

/// Return the SHA of HEAD's parent (HEAD~1).
/// Returns `Ok(None)` if HEAD is the initial commit (no parent).
pub async fn parent_sha() -> Result<Option<String>> {
    let result = run_git(&["rev-parse", "--verify", "HEAD~1"]).await;
    match result {
        Ok(sha) => Ok(Some(sha)),
        Err(e) => {
            let msg = e.to_string();
            // Initial commit has no parent
            if msg.contains("unknown revision")
                || msg.contains("bad revision")
                || msg.contains("Needed a single revision")
            {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Return the repository root directory.
pub async fn repo_root() -> Result<PathBuf> {
    let root = run_git(&["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(root))
}

/// Read the offload-images note for a given commit.
///
/// Returns `Ok(None)` if the notes ref or the note for this commit doesn't exist.
pub async fn read_note(commit_sha: &str) -> Result<Option<NoteContents>> {
    let sha = commit_sha.to_string();
    let args = vec![
        "notes".to_string(),
        "--ref".to_string(),
        NOTES_REF.to_string(),
        "show".to_string(),
        sha,
    ];
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args(&args)
            .output()
            .context("failed to run git notes show")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "no note found" or missing ref -- not an error
            let stderr_lower = stderr.to_lowercase();
            if stderr_lower.contains("no note found") || stderr_lower.contains("not a valid ref") {
                return Ok(None);
            }
            bail!("git notes show failed: {}", stderr.trim());
        }
        let json =
            String::from_utf8(output.stdout).context("git notes output was not valid UTF-8")?;
        let contents: NoteContents =
            serde_json::from_str(&json).context("failed to parse note JSON")?;
        Ok(Some(contents))
    })
    .await?
}

/// Write (or merge) an offload-images note for a given commit.
///
/// Performs a read-modify-write: existing entries for other config keys are
/// preserved. The note is pretty-printed JSON with 4-space indentation.
pub async fn write_note(commit_sha: &str, contents: &NoteContents) -> Result<()> {
    // Read existing note to merge
    let existing = read_note(commit_sha).await?.unwrap_or_default();
    let mut merged = existing;
    for (key, value) in contents {
        merged.insert(key.clone(), value.clone());
    }

    let json = serde_json::to_string_pretty(&merged).context("failed to serialize note JSON")?;
    let sha = commit_sha.to_string();

    tokio::task::spawn_blocking(move || {
        // Write JSON to a temp file and use -F to pass it to git notes add
        let mut tmp = tempfile::NamedTempFile::new().context("failed to create temp file")?;
        std::io::Write::write_all(&mut tmp, json.as_bytes())
            .context("failed to write note content to temp file")?;
        std::io::Write::flush(&mut tmp).context("failed to flush temp file")?;

        let output = std::process::Command::new("git")
            .args([
                "notes",
                "--ref",
                NOTES_REF,
                "add",
                "-f",
                "-F",
                &tmp.path().to_string_lossy(),
                &sha,
            ])
            .output()
            .context("failed to run git notes add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git notes add failed: {}", stderr.trim());
        }
        Ok(())
    })
    .await?
}

/// Force-push notes to a remote.
///
/// Returns `Ok(())` even if the remote ref doesn't exist yet (first push creates it).
pub async fn push_notes(remote: &str) -> Result<()> {
    let remote = remote.to_string();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args(["push", &remote, NOTES_REF, "--force"])
            .output()
            .context("failed to run git push")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git push notes failed: {}", stderr.trim());
        }
        Ok(())
    })
    .await?
}

/// Fetch notes from a remote.
///
/// Returns `Ok(())` even if the remote ref doesn't exist.
pub async fn fetch_notes(remote: &str) -> Result<()> {
    let remote = remote.to_string();
    let refspec = format!("{NOTES_REF}:{NOTES_REF}");
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args(["fetch", &remote, &refspec])
            .output()
            .context("failed to run git fetch")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Missing remote ref is not an error
            if stderr.contains("couldn't find remote ref")
                || stderr.contains("no such ref")
                || stderr.contains("does not appear to be a git repository")
            {
                return Ok(());
            }
            bail!("git fetch notes failed: {}", stderr.trim());
        }
        Ok(())
    })
    .await?
}

/// Ensure the notes refspec is in the remote's fetch configuration.
///
/// Adds `+refs/notes/offload-images:refs/notes/offload-images` to
/// `remote.<remote>.fetch` if not already present.
pub async fn configure_notes_fetch(remote: &str) -> Result<()> {
    let remote = remote.to_string();
    let refspec = format!("+{NOTES_REF}:{NOTES_REF}");

    tokio::task::spawn_blocking(move || {
        // Check existing fetch refspecs
        let output = std::process::Command::new("git")
            .args(["config", "--get-all", &format!("remote.{remote}.fetch")])
            .output()
            .context("failed to run git config")?;

        let existing = String::from_utf8_lossy(&output.stdout);
        if existing.lines().any(|line| line.trim() == refspec) {
            return Ok(());
        }

        // Add the refspec
        let add_output = std::process::Command::new("git")
            .args([
                "config",
                "--add",
                &format!("remote.{remote}.fetch"),
                &refspec,
            ])
            .output()
            .context("failed to run git config --add")?;

        if !add_output.status.success() {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            bail!("git config --add failed: {}", stderr.trim());
        }
        Ok(())
    })
    .await?
}

/// Check whether a commit touches any of the given paths.
///
/// Uses `git diff-tree` with `-m` to handle merge commits (checks all parents).
pub async fn commit_touches_paths(commit_sha: &str, paths: &[String]) -> Result<bool> {
    let sha = commit_sha.to_string();
    let paths = paths.to_vec();

    let output = run_git(&[
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-m",
        &sha,
    ])
    .await?;

    let changed: std::collections::HashSet<&str> = output.lines().collect();
    Ok(paths.iter().any(|p| changed.contains(p.as_str())))
}

/// Export a commit's tree into an existing directory as a shallow git clone.
///
/// Creates a shallow clone (depth=1) of the current repo at the given commit
/// SHA in `dest`. The result is a proper git repository whose HEAD points to
/// the real commit, preserving the actual SHA, author, and message. This is
/// needed so that `COPY . /app` in a Dockerfile includes `.git/`, which many
/// repos require and which the thin-diff `git apply` step depends on.
pub async fn export_tree(commit_sha: &str, dest: &Path) -> Result<()> {
    let repo_root = repo_root().await?;
    let sha = commit_sha.to_string();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // file:// protocol is required to fetch arbitrary SHAs from a local repo.
        let repo_url = format!("file://{}", repo_root.display());

        let run = |args: &[&str]| -> Result<()> {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .with_context(|| format!("failed to run git {}", args.join(" ")))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("git {} failed: {}", args.join(" "), stderr.trim());
            }
            Ok(())
        };

        run(&["init", &dest.to_string_lossy()])?;
        run(&[
            "-C",
            &dest.to_string_lossy(),
            "fetch",
            "--depth=1",
            &repo_url,
            &sha,
        ])?;
        run(&["-C", &dest.to_string_lossy(), "checkout", "FETCH_HEAD"])?;
        // Create a branch so refs/heads/ is non-empty.  Some container
        // image builders (e.g. Modal) only upload files, not empty
        // directories, and git refuses to recognise a repo whose
        // refs/heads/ directory is missing.
        run(&["-C", &dest.to_string_lossy(), "checkout", "-b", "main"])?;

        Ok(())
    })
    .await?
}

/// Count the number of files changed between two commits.
pub async fn diff_file_count(from_sha: &str, to_sha: &str) -> Result<usize> {
    let output = run_git(&["diff", "--name-only", from_sha, to_sha]).await?;
    if output.is_empty() {
        return Ok(0);
    }
    Ok(output.lines().count())
}

/// Return the SHAs of the last `max_depth` ancestors of HEAD (including HEAD).
pub async fn ancestors(max_depth: usize) -> Result<Vec<String>> {
    let n = max_depth.to_string();
    let output = run_git(&["log", "--format=%H", "-n", &n]).await?;
    Ok(output.lines().map(|s| s.to_string()).collect())
}

/// Convert a config path to a canonical repo-relative form.
///
/// Strips `./` prefix and makes the path relative to the repo root.
pub fn canonicalize_config_path(config_path: &str, repo_root: &Path) -> Result<String> {
    let stripped = config_path.strip_prefix("./").unwrap_or(config_path);
    let path = Path::new(stripped);

    if path.is_absolute() {
        // Make absolute paths relative to repo root
        let rel = path.strip_prefix(repo_root).with_context(|| {
            format!(
                "config path {} is not under repo root {}",
                config_path,
                repo_root.display()
            )
        })?;
        Ok(rel.to_string_lossy().to_string())
    } else {
        Ok(stripped.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a temp directory with an initialized git repo.
    fn init_temp_repo() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        let run = |args: &[&str]| -> Result<()> {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("git {} failed: {}", args.join(" "), stderr.trim());
            }
            Ok(())
        };
        run(&["init"])?;
        run(&["config", "user.email", "test@test.com"])?;
        run(&["config", "user.name", "Test"])?;
        // Create an initial commit so HEAD exists
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# test repo")?;
        run(&["add", "README.md"])?;
        run(&["commit", "-m", "initial commit"])?;
        Ok(dir)
    }

    /// Helper: run a git command in a directory and return stdout.
    fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
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

    // ---- Unit tests ----

    #[test]
    fn test_image_entry_json_round_trip() -> Result<()> {
        let entry = ImageEntry {
            image_id: "im-abc123".to_string(),
        };
        let json = serde_json::to_string(&entry)?;
        let parsed: ImageEntry = serde_json::from_str(&json)?;
        assert_eq!(parsed.image_id, entry.image_id);
        Ok(())
    }

    #[test]
    fn test_canonicalize_config_path() -> Result<()> {
        let repo = PathBuf::from("/home/user/project");

        // Strips ./
        assert_eq!(
            canonicalize_config_path("./offload.toml", &repo)?,
            "offload.toml"
        );

        // Already clean
        assert_eq!(
            canonicalize_config_path("offload.toml", &repo)?,
            "offload.toml"
        );

        // Nested path
        assert_eq!(
            canonicalize_config_path("./configs/offload.toml", &repo)?,
            "configs/offload.toml"
        );

        // Nested without ./
        assert_eq!(
            canonicalize_config_path("configs/offload.toml", &repo)?,
            "configs/offload.toml"
        );

        // Absolute path under repo root
        assert_eq!(
            canonicalize_config_path("/home/user/project/offload.toml", &repo)?,
            "offload.toml"
        );
        Ok(())
    }

    // ---- Integration tests (temp git repos) ----

    #[tokio::test]
    async fn test_write_and_read_note() -> Result<()> {
        let dir = init_temp_repo()?;
        let sha = git_in(dir.path(), &["rev-parse", "HEAD"])?;

        let mut contents = NoteContents::new();
        contents.insert(
            "offload.toml".to_string(),
            ImageEntry {
                image_id: "im-test123".to_string(),
            },
        );

        write_note_in(dir.path(), &sha, &contents).await?;
        let read_back = read_note_in(dir.path(), &sha).await?;

        let note = read_back.context("expected note to exist")?;
        assert_eq!(
            note.get("offload.toml").context("missing key")?.image_id,
            "im-test123"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_write_note_merges_configs() -> Result<()> {
        let dir = init_temp_repo()?;
        let sha = git_in(dir.path(), &["rev-parse", "HEAD"])?;

        // Write first config
        let mut contents_a = NoteContents::new();
        contents_a.insert(
            "config-a.toml".to_string(),
            ImageEntry {
                image_id: "im-aaa".to_string(),
            },
        );
        write_note_in(dir.path(), &sha, &contents_a).await?;

        // Write second config -- should merge, not overwrite
        let mut contents_b = NoteContents::new();
        contents_b.insert(
            "config-b.toml".to_string(),
            ImageEntry {
                image_id: "im-bbb".to_string(),
            },
        );
        write_note_in(dir.path(), &sha, &contents_b).await?;

        let read_back = read_note_in(dir.path(), &sha)
            .await?
            .context("expected note")?;
        assert_eq!(read_back.len(), 2);
        assert_eq!(
            read_back
                .get("config-a.toml")
                .context("missing key a")?
                .image_id,
            "im-aaa"
        );
        assert_eq!(
            read_back
                .get("config-b.toml")
                .context("missing key b")?
                .image_id,
            "im-bbb"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_export_tree() -> Result<()> {
        let dir = init_temp_repo()?;

        // Add a file and commit
        std::fs::write(dir.path().join("hello.txt"), "world")?;
        git_in(dir.path(), &["add", "hello.txt"])?;
        git_in(dir.path(), &["commit", "-m", "add hello"])?;
        let sha = git_in(dir.path(), &["rev-parse", "HEAD"])?;

        // Add another file on top (should NOT appear in export of prior commit)
        std::fs::write(dir.path().join("extra.txt"), "nope")?;
        git_in(dir.path(), &["add", "extra.txt"])?;
        git_in(dir.path(), &["commit", "-m", "add extra"])?;

        // Export the earlier commit's tree
        let dest = tempfile::tempdir()?;
        export_tree_in(dir.path(), &sha, dest.path()).await?;

        // The exported tree should contain hello.txt and README.md
        assert_eq!(
            std::fs::read_to_string(dest.path().join("hello.txt"))?,
            "world"
        );
        assert!(dest.path().join("README.md").exists());
        // extra.txt should NOT be present
        assert!(!dest.path().join("extra.txt").exists());
        // Should be a git repo with the actual SHA as HEAD
        assert!(dest.path().join(".git").exists(), ".git should exist");
        let exported_head = git_in(dest.path(), &["rev-parse", "HEAD"])?;
        assert_eq!(exported_head, sha, "HEAD should match the exported SHA");
        Ok(())
    }

    #[tokio::test]
    async fn test_commit_touches_paths() -> Result<()> {
        let dir = init_temp_repo()?;

        // Create a file and commit
        std::fs::write(dir.path().join("Dockerfile"), "FROM ubuntu")?;
        git_in(dir.path(), &["add", "Dockerfile"])?;
        git_in(dir.path(), &["commit", "-m", "add dockerfile"])?;
        let sha = git_in(dir.path(), &["rev-parse", "HEAD"])?;

        let touches =
            commit_touches_paths_in(dir.path(), &sha, &["Dockerfile".to_string()]).await?;
        assert!(touches);

        let no_touch =
            commit_touches_paths_in(dir.path(), &sha, &["nonexistent.txt".to_string()]).await?;
        assert!(!no_touch);
        Ok(())
    }

    #[tokio::test]
    async fn test_commit_touches_paths_merge_commit() -> Result<()> {
        let dir = init_temp_repo()?;

        // Create branch A with file changes
        git_in(dir.path(), &["checkout", "-b", "branch-a"])?;
        std::fs::write(dir.path().join("file-a.txt"), "branch a content")?;
        git_in(dir.path(), &["add", "file-a.txt"])?;
        git_in(dir.path(), &["commit", "-m", "add file-a"])?;

        // Go back to main and create branch B
        git_in(dir.path(), &["checkout", "main"])?;
        git_in(dir.path(), &["checkout", "-b", "branch-b"])?;
        std::fs::write(dir.path().join("file-b.txt"), "branch b content")?;
        git_in(dir.path(), &["add", "file-b.txt"])?;
        git_in(dir.path(), &["commit", "-m", "add file-b"])?;

        // Merge branch-a into branch-b
        git_in(dir.path(), &["merge", "branch-a", "-m", "merge"])?;
        let merge_sha = git_in(dir.path(), &["rev-parse", "HEAD"])?;

        // The merge commit should touch files from branch-a (via -m flag)
        let touches_a =
            commit_touches_paths_in(dir.path(), &merge_sha, &["file-a.txt".to_string()]).await?;
        assert!(touches_a);

        // And files from branch-b
        let touches_b =
            commit_touches_paths_in(dir.path(), &merge_sha, &["file-b.txt".to_string()]).await?;
        assert!(touches_b);
        Ok(())
    }

    #[tokio::test]
    async fn test_configure_notes_fetch_idempotent() -> Result<()> {
        let dir = init_temp_repo()?;

        // Create a bare remote repo
        let remote_dir = tempfile::tempdir()?;
        let output = std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(remote_dir.path())
            .output()?;
        if !output.status.success() {
            bail!("git init --bare failed");
        }

        // Add remote
        git_in(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                &remote_dir.path().to_string_lossy(),
            ],
        )?;

        let refspec = format!("+{NOTES_REF}:{NOTES_REF}");

        // Call configure twice
        configure_notes_fetch_in(dir.path(), "origin").await?;
        configure_notes_fetch_in(dir.path(), "origin").await?;

        // Check that refspec appears exactly once
        let output = git_in(dir.path(), &["config", "--get-all", "remote.origin.fetch"])?;
        let count = output.lines().filter(|l| l.trim() == refspec).count();
        assert_eq!(count, 1, "refspec should appear exactly once");
        Ok(())
    }

    #[tokio::test]
    async fn test_read_note_missing_ref_returns_none() -> Result<()> {
        let dir = init_temp_repo()?;
        let sha = git_in(dir.path(), &["rev-parse", "HEAD"])?;

        let result = read_note_in(dir.path(), &sha).await?;
        assert!(result.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_notes_missing_ref_returns_ok() -> Result<()> {
        let dir = init_temp_repo()?;

        // Create a bare remote repo (no notes ref)
        let remote_dir = tempfile::tempdir()?;
        let output = std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(remote_dir.path())
            .output()?;
        if !output.status.success() {
            bail!("git init --bare failed");
        }

        git_in(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                &remote_dir.path().to_string_lossy(),
            ],
        )?;

        // fetch_notes should return Ok(()) even though remote has no notes ref
        fetch_notes_in(dir.path(), "origin").await?;
        Ok(())
    }

    // ---- Test helpers that operate on a specific directory ----
    // These avoid global GIT_DIR/GIT_WORK_TREE pollution between tests.

    async fn read_note_in(dir: &Path, commit_sha: &str) -> Result<Option<NoteContents>> {
        let dir = dir.to_path_buf();
        let sha = commit_sha.to_string();
        let args = vec![
            "notes".to_string(),
            "--ref".to_string(),
            NOTES_REF.to_string(),
            "show".to_string(),
            sha,
        ];
        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .context("failed to run git notes show")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr_lower = stderr.to_lowercase();
                if stderr_lower.contains("no note found")
                    || stderr_lower.contains("not a valid ref")
                {
                    return Ok(None);
                }
                bail!("git notes show failed: {}", stderr.trim());
            }
            let json =
                String::from_utf8(output.stdout).context("git notes output was not valid UTF-8")?;
            let contents: NoteContents =
                serde_json::from_str(&json).context("failed to parse note JSON")?;
            Ok(Some(contents))
        })
        .await?
    }

    async fn write_note_in(dir: &Path, commit_sha: &str, contents: &NoteContents) -> Result<()> {
        // Read existing to merge
        let existing = read_note_in(dir, commit_sha).await?.unwrap_or_default();
        let mut merged = existing;
        for (key, value) in contents {
            merged.insert(key.clone(), value.clone());
        }
        let json = serde_json::to_string_pretty(&merged)?;

        let dir = dir.to_path_buf();
        let sha = commit_sha.to_string();
        tokio::task::spawn_blocking(move || {
            let mut tmp = tempfile::NamedTempFile::new()?;
            std::io::Write::write_all(&mut tmp, json.as_bytes())?;
            std::io::Write::flush(&mut tmp)?;

            let output = std::process::Command::new("git")
                .args([
                    "notes",
                    "--ref",
                    NOTES_REF,
                    "add",
                    "-f",
                    "-F",
                    &tmp.path().to_string_lossy(),
                    &sha,
                ])
                .current_dir(&dir)
                .output()
                .context("failed to run git notes add")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("git notes add failed: {}", stderr.trim());
            }
            Ok(())
        })
        .await?
    }

    async fn commit_touches_paths_in(
        dir: &Path,
        commit_sha: &str,
        paths: &[String],
    ) -> Result<bool> {
        let sha = commit_sha.to_string();
        let output = run_git_in(
            dir,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-m",
                &sha,
            ],
        )
        .await?;

        let changed: std::collections::HashSet<&str> = output.lines().collect();
        Ok(paths.iter().any(|p| changed.contains(p.as_str())))
    }

    async fn configure_notes_fetch_in(dir: &Path, remote: &str) -> Result<()> {
        let dir = dir.to_path_buf();
        let remote = remote.to_string();
        let refspec = format!("+{NOTES_REF}:{NOTES_REF}");

        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("git")
                .args(["config", "--get-all", &format!("remote.{remote}.fetch")])
                .current_dir(&dir)
                .output()
                .context("failed to run git config")?;

            let existing = String::from_utf8_lossy(&output.stdout);
            if existing.lines().any(|line| line.trim() == refspec) {
                return Ok(());
            }

            let add_output = std::process::Command::new("git")
                .args([
                    "config",
                    "--add",
                    &format!("remote.{remote}.fetch"),
                    &refspec,
                ])
                .current_dir(&dir)
                .output()
                .context("failed to run git config --add")?;

            if !add_output.status.success() {
                let stderr = String::from_utf8_lossy(&add_output.stderr);
                bail!("git config --add failed: {}", stderr.trim());
            }
            Ok(())
        })
        .await?
    }

    async fn export_tree_in(repo_dir: &Path, commit_sha: &str, dest: &Path) -> Result<()> {
        let repo_dir = repo_dir.to_path_buf();
        let sha = commit_sha.to_string();
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo_url = format!("file://{}", repo_dir.display());

            let run = |args: &[&str]| -> Result<()> {
                let output = std::process::Command::new("git")
                    .args(args)
                    .output()
                    .with_context(|| format!("failed to run git {}", args.join(" ")))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    bail!("git {} failed: {}", args.join(" "), stderr.trim());
                }
                Ok(())
            };

            run(&["init", &dest.to_string_lossy()])?;
            run(&[
                "-C",
                &dest.to_string_lossy(),
                "fetch",
                "--depth=1",
                &repo_url,
                &sha,
            ])?;
            run(&["-C", &dest.to_string_lossy(), "checkout", "FETCH_HEAD"])?;
            run(&["-C", &dest.to_string_lossy(), "checkout", "-b", "main"])?;

            Ok(())
        })
        .await?
    }

    async fn fetch_notes_in(dir: &Path, remote: &str) -> Result<()> {
        let dir = dir.to_path_buf();
        let remote = remote.to_string();
        let refspec = format!("{NOTES_REF}:{NOTES_REF}");
        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("git")
                .args(["fetch", &remote, &refspec])
                .current_dir(&dir)
                .output()
                .context("failed to run git fetch")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("couldn't find remote ref")
                    || stderr.contains("no such ref")
                    || stderr.contains("does not appear to be a git repository")
                {
                    return Ok(());
                }
                bail!("git fetch notes failed: {}", stderr.trim());
            }
            Ok(())
        })
        .await?
    }

    async fn parent_sha_in(dir: &Path) -> Result<Option<String>> {
        let result = run_git_in(dir, &["rev-parse", "--verify", "HEAD~1"]).await;
        match result {
            Ok(sha) => Ok(Some(sha)),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("unknown revision")
                    || msg.contains("bad revision")
                    || msg.contains("Needed a single revision")
                {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    #[tokio::test]
    async fn test_parent_sha_exists() -> Result<()> {
        let dir = init_temp_repo()?;
        // init_temp_repo creates one commit. Add another.
        std::fs::write(dir.path().join("second.txt"), "second")?;
        git_in(dir.path(), &["add", "second.txt"])?;
        git_in(dir.path(), &["commit", "-m", "second commit"])?;

        let parent = parent_sha_in(dir.path()).await?;
        assert!(parent.is_some(), "should have a parent");

        // Parent should be the initial commit, not the current HEAD
        let head = git_in(dir.path(), &["rev-parse", "HEAD"])?;
        assert_ne!(parent.unwrap(), head);
        Ok(())
    }

    #[tokio::test]
    async fn test_parent_sha_initial_commit() -> Result<()> {
        let dir = init_temp_repo()?;
        // init_temp_repo creates exactly one commit — it has no parent
        let parent = parent_sha_in(dir.path()).await?;
        assert!(parent.is_none(), "initial commit should have no parent");
        Ok(())
    }
}
