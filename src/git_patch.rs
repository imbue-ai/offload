//! Git patch generation and repository snapshot logic.
//!
//! Generates a `git diff` from a base commit, writes it to a temp directory,
//! and creates a tarball snapshot of the repo at that commit.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use tracing::info;

const BASE_COMMIT_FILE: &str = ".offload-base-commit";
const REMOTE_DIR: &str = "/offload-patch";

/// Artifact produced by git patch preparation. Holds temp resources
/// that are cleaned up on drop.
pub struct GitPatchArtifact {
    /// Extra copy_dir to inject: (local_path, remote_path).
    pub copy_dir: Option<(PathBuf, PathBuf)>,

    /// Command to prepend to sandbox_init_cmd.
    pub apply_cmd: Option<String>,

    /// Temp dir holding the patch file. Kept alive for RAII cleanup.
    _patch_dir: Option<TempDir>,

    /// Path to snapshot tarball placed in CWD. Cleaned up on Drop.
    snapshot_path: Option<PathBuf>,
}

impl Drop for GitPatchArtifact {
    fn drop(&mut self) {
        if let Some(ref path) = self.snapshot_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Reads the base commit from the base commit file.
///
/// Reads from `.offload-base-commit`. If the file does not exist,
/// bootstraps it with `git rev-parse HEAD`.
fn resolve_base_commit() -> Result<String> {
    let path = BASE_COMMIT_FILE;
    let file_path = std::path::Path::new(path);
    if !file_path.exists() {
        // Bootstrap: create the file with current HEAD
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
        std::fs::write(file_path, format!("{head}\n"))
            .with_context(|| format!("failed to create base_commit_file: {path}"))?;
        info!(
            "[git_patch] Created {} with current HEAD ({})",
            path,
            &head[..head.len().min(12)]
        );
        return Ok(head);
    }

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read base_commit_file: {path}"))?;
    let trimmed = contents.trim().to_string();
    if trimmed.is_empty() {
        bail!("base_commit_file '{path}' is empty");
    }
    Ok(trimmed)
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

/// Creates a tarball of the repository at the given commit, placed in CWD as `current.tar.gz`.
fn create_snapshot(commit: &str) -> Result<PathBuf> {
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

    // Create tarball in CWD
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let tar_path = cwd.join("current.tar.gz");

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
    Ok(tar_path)
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
/// Resolves the base commit, creates a snapshot tarball,
/// and generates a diff patch for application in the sandbox.
pub fn prepare(dockerfile: Option<&str>, sandbox_project_root: &str) -> Result<GitPatchArtifact> {
    let commit = validate_commit(&resolve_base_commit()?)?;

    // Create snapshot tarball
    let snapshot_path = Some(create_snapshot(&commit)?);

    warn_if_dockerfile_missing_snapshot(dockerfile, sandbox_project_root);

    // Generate patch
    let (patch_dir, copy_dir, apply_cmd) = generate_patch(&commit)?;

    Ok(GitPatchArtifact {
        copy_dir,
        apply_cmd,
        _patch_dir: patch_dir,
        snapshot_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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
    fn test_artifact_drop_cleans_snapshot() -> Result<()> {
        let dir = TempDir::new()?;
        let snap = dir.path().join("current.tar.gz");
        std::fs::File::create(&snap)?;
        assert!(snap.exists());

        let artifact = GitPatchArtifact {
            copy_dir: None,
            apply_cmd: None,
            _patch_dir: None,
            snapshot_path: Some(snap.clone()),
        };
        drop(artifact);
        assert!(!snap.exists(), "snapshot file should be removed on drop");
        Ok(())
    }

    #[test]
    fn test_artifact_drop_no_snapshot() {
        let artifact = GitPatchArtifact {
            copy_dir: None,
            apply_cmd: None,
            _patch_dir: None,
            snapshot_path: None,
        };
        drop(artifact); // should not panic
    }

    // ── resolve_base_commit ──

    #[test]
    fn test_resolve_base_commit_from_file() -> Result<()> {
        let (dir, head) = setup_temp_git_repo()?;
        // Write the commit hash to .offload-base-commit
        let base_file = dir.path().join(BASE_COMMIT_FILE);
        std::fs::write(&base_file, format!("{head}\n"))?;

        let result = with_cwd(dir.path(), resolve_base_commit)?;
        assert_eq!(result?, head);
        Ok(())
    }

    #[test]
    fn test_resolve_base_commit_bootstraps_file() -> Result<()> {
        let (dir, head) = setup_temp_git_repo()?;
        let base_file = dir.path().join(BASE_COMMIT_FILE);
        assert!(!base_file.exists());

        let result = with_cwd(dir.path(), resolve_base_commit)?;
        assert_eq!(result?, head);
        assert!(base_file.exists(), "file should be created by bootstrap");
        let contents = std::fs::read_to_string(&base_file)?;
        assert_eq!(contents.trim(), head);
        Ok(())
    }

    #[test]
    fn test_resolve_base_commit_empty_file_errors() -> Result<()> {
        let (dir, _head) = setup_temp_git_repo()?;
        let base_file = dir.path().join(BASE_COMMIT_FILE);
        let mut f = std::fs::File::create(&base_file)?;
        f.write_all(b"")?;

        let result = with_cwd(dir.path(), resolve_base_commit)?;
        assert!(result.is_err(), "empty file should produce an error");
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
