//! Git patch generation and repository snapshot logic.
//!
//! Generates a `git diff` from a base commit, writes it to a temp directory,
//! and creates a tarball snapshot of the repo at that commit.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use tracing::info;

const REMOTE_DIR: &str = "/offload-patch";

/// Artifact produced by git patch preparation. Holds temp resources
/// that are cleaned up on drop.
pub struct GitPatchArtifact {
    /// Extra copy_dir to inject: (local_path, remote_path).
    pub copy_dir: Option<(PathBuf, PathBuf)>,

    /// Command to prepend to sandbox_init_cmd.
    pub apply_cmd: Option<String>,

    /// The resolved base commit hash.
    pub base_commit: String,

    /// Temp dir holding the patch file. Kept alive for RAII cleanup.
    _patch_dir: Option<TempDir>,

    /// Temp dir holding the snapshot tarball. Kept alive for RAII cleanup.
    _snapshot_dir: Option<TempDir>,

    /// Docker build context directory (the snapshot temp dir path).
    pub context_dir: Option<PathBuf>,
}

/// Resolves the base commit from dependency file history.
///
/// Builds a combined file list from `dependencies` and `dockerfile` (if
/// provided). If the combined list is empty, returns HEAD. Otherwise,
/// returns the most recent commit that modified any of the listed files.
fn resolve_base_commit(dependencies: &[String], dockerfile: Option<&str>) -> Result<String> {
    let mut files: Vec<&str> = dependencies.iter().map(String::as_str).collect();
    if let Some(df) = dockerfile {
        files.push(df);
    }

    if files.is_empty() {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .context("failed to run git rev-parse HEAD")?;
        if !output.status.success() {
            bail!(
                "git rev-parse HEAD failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        info!("[git_patch] No dependency files configured; using HEAD ({head})");
        return Ok(head);
    }

    let mut cmd = Command::new("git");
    cmd.args(["log", "-1", "--format=%H", "--"]);
    for f in &files {
        cmd.arg(f);
    }
    let output = cmd.output().context("failed to run git log")?;
    if !output.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hash.is_empty() {
        bail!(
            "no commit found that modified any of the dependency files: {}",
            files.join(", ")
        );
    }
    info!("[git_patch] Resolved base commit {hash} from dependency files");
    Ok(hash)
}

/// Warns if any of the given files have uncommitted changes (staged or unstaged).
fn warn_uncommitted_changes(files: &[&str]) {
    if files.is_empty() {
        return;
    }

    let check = |diff_args: &[&str], label: &str| {
        let mut cmd = Command::new("git");
        cmd.args(diff_args);
        cmd.arg("--");
        for f in files {
            cmd.arg(f);
        }
        if let Ok(output) = cmd.output() {
            let changed: Vec<&str> = std::str::from_utf8(&output.stdout)
                .unwrap_or("")
                .lines()
                .filter(|l| !l.is_empty())
                .collect();
            for path in changed {
                tracing::warn!("[git_patch] Dependency file has {label} changes: {path}");
            }
        }
    };

    check(&["diff", "--name-only"], "unstaged");
    check(&["diff", "--name-only", "--staged"], "staged");
}

/// Validates that a commit exists in the current repository and returns the full hash.
fn validate_commit(commit: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", commit])
        .output()
        .context("failed to run git rev-parse")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("commit '{commit}' does not exist: {stderr}");
    }

    let hash = String::from_utf8(output.stdout)
        .context("git rev-parse output is not valid UTF-8")?
        .trim()
        .to_string();
    Ok(hash)
}

/// Result of generating a git diff patch.
type PatchResult = (Option<TempDir>, Option<(PathBuf, PathBuf)>, Option<String>);

/// Generates a git diff patch from the given commit to HEAD.
///
/// Returns `(patch_dir, copy_dir, apply_cmd)`. If the diff is empty (no changes),
/// all three are `None`.
fn generate_patch(commit: &str) -> Result<PatchResult> {
    let output = Command::new("git")
        .args(["diff", commit])
        .output()
        .context("failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff failed: {stderr}");
    }

    if output.stdout.is_empty() {
        info!("no changes from base commit; skipping patch");
        return Ok((None, None, None));
    }

    let patch_dir = TempDir::new().context("failed to create temp dir for patch")?;
    let patch_path = patch_dir.path().join("patch");
    std::fs::write(&patch_path, &output.stdout).context("failed to write patch file")?;

    let copy_dir = (patch_dir.path().to_path_buf(), PathBuf::from(REMOTE_DIR));
    let apply_cmd = format!("git apply {REMOTE_DIR}/patch --allow-empty");

    info!(
        patch_bytes = output.stdout.len(),
        "generated patch from {commit}"
    );

    Ok((Some(patch_dir), Some(copy_dir), Some(apply_cmd)))
}

/// Creates a tarball of the repository at the given commit in a new temp directory.
///
/// Returns `(snapshot_dir, tar_path)` where `snapshot_dir` is the TempDir
/// owning the tarball and `tar_path` is the path to `current.tar.gz` inside it.
fn create_snapshot(commit: &str) -> Result<(TempDir, PathBuf)> {
    let clone_dir = TempDir::new().context("failed to create temp dir for snapshot clone")?;
    let clone_path = clone_dir.path().join("repo");

    // Fast local clone
    let status = Command::new("git")
        .args(["clone", ".", clone_path.to_str().unwrap_or(".")])
        .status()
        .context("failed to run git clone")?;
    if !status.success() {
        bail!("git clone failed");
    }

    // Fix remote URL: get real origin URL from the source repo
    let origin_output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .context("failed to get origin URL")?;

    if origin_output.status.success() {
        let origin_url = String::from_utf8(origin_output.stdout)
            .context("origin URL is not valid UTF-8")?
            .trim()
            .to_string();

        let set_status = Command::new("git")
            .args(["-C"])
            .arg(&clone_path)
            .args(["remote", "set-url", "origin", &origin_url])
            .status()
            .context("failed to set origin URL on clone")?;
        if !set_status.success() {
            bail!("failed to set origin URL on clone");
        }
    }

    // Checkout the base commit
    let checkout_status = Command::new("git")
        .arg("-C")
        .arg(&clone_path)
        .args(["checkout", commit])
        .status()
        .context("failed to checkout commit")?;
    if !checkout_status.success() {
        bail!("git checkout {commit} failed in clone");
    }

    // Create tarball in a dedicated temp directory (not CWD)
    let snapshot_dir = TempDir::new().context("failed to create temp dir for snapshot tarball")?;
    let tar_path = snapshot_dir.path().join("current.tar.gz");

    let mut tar_cmd = Command::new("tar");
    tar_cmd.args(["czf"]);
    tar_cmd.arg(&tar_path);
    tar_cmd.arg("-C");
    tar_cmd.arg(&clone_path);
    tar_cmd.arg(".");

    // On macOS, disable extended attributes in tarball
    if cfg!(target_os = "macos") {
        tar_cmd.env("COPYFILE_DISABLE", "1");
    }

    let tar_status = tar_cmd.status().context("failed to run tar")?;
    if !tar_status.success() {
        bail!("tar failed to create snapshot tarball");
    }

    info!("created snapshot tarball at {}", tar_path.display());

    // clone_dir is dropped here, cleaning up the temp clone
    Ok((snapshot_dir, tar_path))
}

/// Checks the Dockerfile for a reference to `current.tar.gz` and warns if missing.
fn warn_if_dockerfile_missing_snapshot(dockerfile: Option<&str>, sandbox_project_root: &str) {
    let hint = format!(
        "Consider adding these lines to your Dockerfile:\n\
         \n  COPY current.tar.gz /code/current.tar.gz\
         \n  RUN mkdir -p {spr} && tar xzf /code/current.tar.gz -C {spr} \
         && rm /code/current.tar.gz\n",
        spr = sandbox_project_root,
    );

    let Some(path) = dockerfile else {
        tracing::warn!(
            "[git_patch] No Dockerfile configured. \
             [git_patch] requires a Dockerfile that extracts current.tar.gz. {hint}"
        );
        return;
    };

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // Can't read the Dockerfile, skip check
    };
    if !contents.contains("current.tar.gz") {
        tracing::warn!("[git_patch] Dockerfile '{path}' does not reference current.tar.gz. {hint}");
    }
}

/// Prepares git patch artifacts for sandbox deployment.
///
/// Resolves the base commit from dependency file history, creates a
/// snapshot tarball, and generates a diff patch for application in the
/// sandbox.
pub fn prepare(
    dependencies: &[String],
    dockerfile: Option<&str>,
    sandbox_project_root: &str,
) -> Result<GitPatchArtifact> {
    let commit = validate_commit(&resolve_base_commit(dependencies, dockerfile)?)?;

    // Warn about uncommitted dependency file changes
    let mut dep_files: Vec<&str> = dependencies.iter().map(String::as_str).collect();
    if let Some(df) = dockerfile {
        dep_files.push(df);
    }
    warn_uncommitted_changes(&dep_files);

    // Create snapshot tarball in a temp directory
    let (snapshot_dir, _tar_path) = create_snapshot(&commit)?;
    let context_dir = Some(snapshot_dir.path().to_path_buf());

    warn_if_dockerfile_missing_snapshot(dockerfile, sandbox_project_root);

    // Generate patch
    let (patch_dir, copy_dir, apply_cmd) = generate_patch(&commit)?;

    Ok(GitPatchArtifact {
        copy_dir,
        apply_cmd,
        base_commit: commit,
        _patch_dir: patch_dir,
        _snapshot_dir: Some(snapshot_dir),
        context_dir,
    })
}

/// Reads cached image ID from a git note on the given commit.
/// Uses refs/notes/offload/image-cache namespace.
/// Returns `Some(image_id)` if note exists and starts with "im-".
pub fn read_cached_image_id(base_commit: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["notes", "--ref=offload/image-cache", "show", base_commit])
        .output()
        .context("failed to run git notes show")?;
    if !output.status.success() {
        // No note exists — not an error, just no cache
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.starts_with("im-") {
        Ok(Some(id))
    } else {
        Ok(None)
    }
}

/// Writes an image ID as a git note on the given commit.
/// Uses refs/notes/offload/image-cache namespace.
pub fn write_cached_image_id(base_commit: &str, image_id: &str) -> Result<()> {
    let status = Command::new("git")
        .args([
            "notes",
            "--ref=offload/image-cache",
            "add",
            "-f",
            "-m",
            image_id,
            base_commit,
        ])
        .status()
        .context("failed to run git notes add")?;
    if !status.success() {
        bail!("git notes add failed for commit {base_commit}");
    }
    info!(
        "[git_patch] Cached image {image_id} on commit {}",
        &base_commit[..base_commit.len().min(12)]
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Process-global lock for tests that change the current working directory.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// Creates a temporary git repo with an initial commit.
    /// Returns `(TempDir, HEAD_hash)`.
    fn setup_temp_git_repo() -> Result<(TempDir, String)> {
        let dir = TempDir::new().context("create temp dir")?;
        let path = dir.path().to_path_buf();

        let run = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&path)
                .output()
                .context("git command failed to run")?;
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)
                .context("non-utf8 output")?
                .trim()
                .to_string())
        };

        run(&["init"])?;
        run(&["config", "user.name", "Test"])?;
        run(&["config", "user.email", "test@test.com"])?;

        // Create an initial commit
        let file = path.join("README");
        std::fs::write(&file, "init\n").context("write file")?;
        run(&["add", "."])?;
        run(&["commit", "-m", "initial"])?;

        let head = run(&["rev-parse", "HEAD"])?;
        Ok((dir, head))
    }

    /// Helper that changes CWD under the global lock, runs a closure, then restores CWD.
    fn with_cwd<F, R>(dir: &std::path::Path, f: F) -> Result<R>
    where
        F: FnOnce() -> R,
    {
        let _guard = CWD_LOCK
            .lock()
            .map_err(|e| anyhow::anyhow!("mutex poisoned: {e}"))?;
        let original = std::env::current_dir().context("get cwd")?;
        std::env::set_current_dir(dir).context("set cwd")?;
        let result = f();
        std::env::set_current_dir(original).context("restore cwd")?;
        Ok(result)
    }

    // ── warn_if_dockerfile_missing_snapshot ──

    #[test]
    fn test_warn_no_dockerfile() {
        // Should not panic when dockerfile is None.
        warn_if_dockerfile_missing_snapshot(None, "/app");
    }

    #[test]
    fn test_warn_dockerfile_missing_tar_ref() -> Result<()> {
        let dir = TempDir::new()?;
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM ubuntu:22.04\nRUN echo hello\n")?;
        let path_str = dockerfile.to_str().context("non-utf8 dockerfile path")?;
        warn_if_dockerfile_missing_snapshot(Some(path_str), "/app");
        Ok(())
    }

    #[test]
    fn test_warn_dockerfile_with_tar_ref() -> Result<()> {
        let dir = TempDir::new()?;
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM ubuntu:22.04\nCOPY current.tar.gz /code/current.tar.gz\n",
        )?;
        let path_str = dockerfile.to_str().context("non-utf8 dockerfile path")?;
        warn_if_dockerfile_missing_snapshot(Some(path_str), "/app");
        Ok(())
    }

    #[test]
    fn test_warn_dockerfile_nonexistent_path() {
        // A path that does not exist should silently skip (no panic).
        warn_if_dockerfile_missing_snapshot(Some("/nonexistent/Dockerfile"), "/app");
    }

    // ── GitPatchArtifact Drop ──

    #[test]
    fn test_artifact_drop_cleans_snapshot_dir() -> Result<()> {
        let snapshot_dir = TempDir::new()?;
        let snap = snapshot_dir.path().join("current.tar.gz");
        std::fs::File::create(&snap)?;
        assert!(snap.exists());

        let context_dir = Some(snapshot_dir.path().to_path_buf());
        let artifact = GitPatchArtifact {
            copy_dir: None,
            apply_cmd: None,
            base_commit: String::new(),
            _patch_dir: None,
            _snapshot_dir: Some(snapshot_dir),
            context_dir,
        };
        let dir_path = artifact.context_dir.clone();
        drop(artifact);
        // TempDir removes the directory on drop
        assert!(
            !dir_path.as_ref().is_some_and(|p| p.exists()),
            "snapshot dir should be removed on drop"
        );
        Ok(())
    }

    #[test]
    fn test_artifact_drop_no_snapshot() {
        let artifact = GitPatchArtifact {
            copy_dir: None,
            apply_cmd: None,
            base_commit: String::new(),
            _patch_dir: None,
            _snapshot_dir: None,
            context_dir: None,
        };
        drop(artifact); // should not panic
    }

    // ── resolve_base_commit ──

    #[test]
    fn test_resolve_base_commit_empty_deps_no_dockerfile() -> Result<()> {
        let (dir, head) = setup_temp_git_repo()?;
        let result = with_cwd(dir.path(), || resolve_base_commit(&[], None))?;
        assert_eq!(result?, head);
        Ok(())
    }

    #[test]
    fn test_resolve_base_commit_finds_correct_commit() -> Result<()> {
        let (dir, first_commit) = setup_temp_git_repo()?;
        let path = dir.path().to_path_buf();

        let run = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&path)
                .output()
                .context("git command failed to run")?;
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)
                .context("non-utf8 output")?
                .trim()
                .to_string())
        };

        // Create requirements.txt in first commit (amend initial)
        std::fs::write(path.join("requirements.txt"), "flask\n")?;
        run(&["add", "requirements.txt"])?;
        run(&["commit", "-m", "add requirements"])?;
        let req_commit = run(&["rev-parse", "HEAD"])?;

        // Create a second commit that does NOT touch requirements.txt
        std::fs::write(path.join("README"), "updated\n")?;
        run(&["add", "README"])?;
        run(&["commit", "-m", "update readme"])?;

        let deps = vec!["requirements.txt".to_string()];
        let result = with_cwd(dir.path(), || resolve_base_commit(&deps, None))?;
        assert_eq!(result?, req_commit);
        // Verify it's not the first commit (README-only) or HEAD
        assert_ne!(req_commit, first_commit);
        Ok(())
    }

    #[test]
    fn test_resolve_base_commit_includes_dockerfile() -> Result<()> {
        let (dir, _first_commit) = setup_temp_git_repo()?;
        let path = dir.path().to_path_buf();

        let run = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .args(args)
                .current_dir(&path)
                .output()
                .context("git command failed to run")?;
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)
                .context("non-utf8 output")?
                .trim()
                .to_string())
        };

        // Commit a Dockerfile
        std::fs::write(path.join("Dockerfile"), "FROM ubuntu\n")?;
        run(&["add", "Dockerfile"])?;
        run(&["commit", "-m", "add dockerfile"])?;
        let df_commit = run(&["rev-parse", "HEAD"])?;

        // Another commit that does not touch the Dockerfile
        std::fs::write(path.join("README"), "updated again\n")?;
        run(&["add", "README"])?;
        run(&["commit", "-m", "update readme"])?;

        let result = with_cwd(dir.path(), || resolve_base_commit(&[], Some("Dockerfile")))?;
        assert_eq!(result?, df_commit);
        Ok(())
    }

    #[test]
    fn test_resolve_base_commit_nonexistent_files_returns_error() -> Result<()> {
        let (dir, _head) = setup_temp_git_repo()?;
        let deps = vec!["nonexistent_file.txt".to_string()];
        let result = with_cwd(dir.path(), || resolve_base_commit(&deps, None))?;
        assert!(
            result.is_err(),
            "should error when no commit modified any listed file"
        );
        Ok(())
    }

    // ── validate_commit ──

    #[test]
    fn test_validate_commit_valid() -> Result<()> {
        let (dir, head) = setup_temp_git_repo()?;
        let result = with_cwd(dir.path(), || validate_commit(&head))?;
        assert_eq!(result?, head);
        Ok(())
    }

    #[test]
    fn test_validate_commit_invalid() -> Result<()> {
        let (dir, _head) = setup_temp_git_repo()?;
        // Use a short ref that git will actually try to look up and fail.
        let result = with_cwd(dir.path(), || validate_commit("nonexistent_ref"))?;
        assert!(result.is_err());
        Ok(())
    }

    // ── generate_patch ──

    #[test]
    fn test_generate_patch_no_changes() -> Result<()> {
        let (dir, head) = setup_temp_git_repo()?;
        let (patch_dir, copy_dir, apply_cmd) = with_cwd(dir.path(), || generate_patch(&head))??;
        assert!(patch_dir.is_none());
        assert!(copy_dir.is_none());
        assert!(apply_cmd.is_none());
        Ok(())
    }

    #[test]
    fn test_generate_patch_with_changes() -> Result<()> {
        let (dir, head) = setup_temp_git_repo()?;
        // Modify a file to create a diff
        let readme = dir.path().join("README");
        std::fs::write(&readme, "modified\n")?;

        let (patch_dir, copy_dir, apply_cmd) = with_cwd(dir.path(), || generate_patch(&head))??;

        assert!(patch_dir.is_some(), "patch_dir should be Some");
        assert!(copy_dir.is_some(), "copy_dir should be Some");
        assert!(apply_cmd.is_some(), "apply_cmd should be Some");

        // Verify the patch file exists and is non-empty
        let pd = patch_dir.context("patch_dir was None")?;
        let patch_file = pd.path().join("patch");
        assert!(patch_file.exists(), "patch file should exist");
        let metadata = std::fs::metadata(&patch_file)?;
        assert!(metadata.len() > 0, "patch file should be non-empty");
        Ok(())
    }
}
