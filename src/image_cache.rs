//! Image cache resolution and orchestration for sandbox image caching via git notes.
//!
//! This module resolves checkpoint commits and their cached images by reading
//! git notes, resolves base commits for the caching pipeline, and writes
//! cache notes after image builds.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;
use crate::config::schema::CheckpointConfig;
use crate::git;
use crate::provider::SandboxProvider;
use crate::trace::Tracer;
use crate::with_retry;

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
/// Returns the SHA of the most recent ancestor of HEAD (within `max_depth`)
/// that touches any of the configured `build_inputs` paths, or `None` if no
/// such commit exists.
pub async fn find_checkpoint_sha(
    repo: &Path,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<String>> {
    git::nearest_ancestor_touching(repo, &checkpoint_cfg.build_inputs, max_depth).await
}

/// Find the nearest checkpoint ancestor and its cached image information.
///
/// Returns `None` if no ancestor within `max_depth` touches any `build_inputs`.
/// Returns `Some(CheckpointInfo { cached_image: None })` if a checkpoint commit
/// is found but has no cached image in git notes.
pub async fn resolve_checkpoint(
    repo: &Path,
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<CheckpointInfo>> {
    let checkpoint_sha = match git::nearest_ancestor_touching(
        repo,
        &checkpoint_cfg.build_inputs,
        max_depth,
    )
    .await?
    {
        Some(sha) => sha,
        None => return Ok(None),
    };

    let repo_root = git::repo_root(repo).await?;
    let config_key = git::canonicalize_config_path(config_path, &repo_root)?;
    let cached_image = read_cached_image_for_commit(repo, &checkpoint_sha, &config_key).await?;

    Ok(Some(CheckpointInfo {
        checkpoint_sha,
        cached_image,
    }))
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

impl fmt::Display for BaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Checkpoint => f.write_str("Checkpoint"),
            Self::LatestCommit => f.write_str("Latest-commit"),
        }
    }
}

/// Pre-resolved base commit and its cached image, determined before provider dispatch.
pub struct ResolvedBase {
    pub base_sha: String,
    pub cached_image_id: Option<String>,
    pub kind: BaseKind,
}

/// Context for the image cache prewarm pipeline.
pub struct PrewarmContext<'a> {
    pub repo: &'a Path,
    pub config: &'a crate::config::Config,
    pub config_path: &'a Path,
    pub copy_dir_tuples: &'a [(PathBuf, PathBuf)],
    pub no_cache: bool,
    pub tracer: &'a Tracer,
    pub discovery_done: &'a AtomicBool,
}

/// Outcome of the image cache prewarm pipeline.
pub enum PrewarmOutcome {
    /// An image was resolved (from cache hit + thin diff, or base build + thin diff).
    Resolved { image_id: String },
    /// Prewarm could not produce an image. Caller should fall back to full build.
    /// Contains the resolved base SHA (if any) so the caller can write a cache
    /// note after a full build without re-resolving.
    CacheMiss { base_sha: Option<String> },
}

/// Resolve the base commit before provider dispatch.
///
/// When `no_cache` is false, fetches git notes and includes cached image IDs.
/// When `no_cache` is true, resolves only the base SHA (no notes fetch/lookup).
///
/// Returns `None` if not in a git repo, if resolution fails (best-effort), or if
/// there are no commits (non-checkpoint workflow).
pub async fn resolve_base(
    repo: &Path,
    config_path: &Path,
    config: &Config,
    no_cache: bool,
) -> Option<ResolvedBase> {
    // Check if we're in a git repo
    if git::repo_root(repo).await.is_err() {
        info!("Not in a git repo, skipping checkpoint/cache resolution");
        return None;
    }

    if !no_cache {
        // Best-effort fetch and configure notes
        if let Err(e) = git::fetch_notes(repo, "origin").await {
            warn!("Failed to fetch notes: {}", e);
        }
        if let Err(e) = git::configure_notes_fetch(repo, "origin").await {
            warn!("Failed to configure notes fetch: {}", e);
        }
    }

    // Checkpoint caching: if we have a [checkpoint] section, use checkpoint-based caching
    if let Some(checkpoint_cfg) = config.checkpoint.as_ref() {
        if no_cache {
            // Resolve SHA only, no notes lookup
            return match find_checkpoint_sha(repo, checkpoint_cfg, 100).await {
                Ok(Some(sha)) => Some(ResolvedBase {
                    base_sha: sha,
                    cached_image_id: None,
                    kind: BaseKind::Checkpoint,
                }),
                Ok(None) => {
                    info!("No checkpoint commit found in ancestor window");
                    None
                }
                Err(e) => {
                    warn!("Checkpoint resolution failed (--no-cache): {}", e);
                    None
                }
            };
        }

        let config_path_str = config_path.to_string_lossy();
        return match resolve_checkpoint(repo, &config_path_str, checkpoint_cfg, 100).await {
            Ok(Some(info)) => Some(ResolvedBase {
                base_sha: info.checkpoint_sha,
                cached_image_id: info.cached_image.map(|c| c.image_id),
                kind: BaseKind::Checkpoint,
            }),
            Ok(None) => {
                info!("No checkpoint commit found in ancestor window");
                None
            }
            Err(e) => {
                warn!("Checkpoint resolution failed: {}", e);
                None
            }
        };
    }

    // Latest-commit caching: no [checkpoint] config — use HEAD as base
    if no_cache {
        // Resolve SHA only, no notes lookup
        return match git::head_sha(repo).await {
            Ok(sha) => Some(ResolvedBase {
                base_sha: sha,
                cached_image_id: None,
                kind: BaseKind::LatestCommit,
            }),
            Err(e) => {
                warn!("HEAD SHA resolution failed (--no-cache): {}", e);
                None
            }
        };
    }

    let config_path_str = config_path.to_string_lossy();
    match resolve_latest_commit(repo, &config_path_str).await {
        Ok(Some(info)) => Some(ResolvedBase {
            base_sha: info.head_sha,
            cached_image_id: info.cached_image.map(|c| c.image_id),
            kind: BaseKind::LatestCommit,
        }),
        Ok(None) => {
            info!("No commits found, skipping latest-commit caching");
            None
        }
        Err(e) => {
            warn!("Latest-commit resolution failed: {}", e);
            None
        }
    }
}

/// Write a git note for an image on a specific commit (best-effort).
pub async fn write_note_for_commit(
    repo: &Path,
    commit_sha: &str,
    image_id: &str,
    config_path: &Path,
) {
    let config_path_str = config_path.to_string_lossy();

    let config_key = match git::repo_root(repo).await {
        Ok(root) => match git::canonicalize_config_path(&config_path_str, &root) {
            Ok(key) => key,
            Err(e) => {
                warn!("Failed to canonicalize config path for note: {}", e);
                return;
            }
        },
        Err(e) => {
            warn!("Failed to get repo root for note: {}", e);
            return;
        }
    };

    let mut contents = git::NoteContents::new();
    contents.insert(
        config_key,
        git::ImageEntry {
            image_id: image_id.to_string(),
        },
    );

    if let Err(e) = git::write_note(repo, commit_sha, &contents).await {
        warn!("Failed to write note: {}", e);
        return;
    }
    info!(
        "Wrote image cache note on {}",
        &commit_sha[..8.min(commit_sha.len())]
    );

    if let Err(e) = git::push_notes(repo, "origin").await {
        warn!("Failed to push notes: {}", e);
    }
}

/// Run the image cache prewarm pipeline (stages 1 and 2).
///
/// Stage 1: If a cached image exists, generate a thin diff and build on top.
/// Stage 2: If no cache, export the base tree, build a base image via
///          `provider.prepare()`, write a cache note, then build a thin diff.
///
/// Returns `Resolved` if an image was produced, `CacheMiss` if the caller
/// should fall through to a full build.
pub async fn run_prewarm_pipeline<P: SandboxProvider>(
    provider: &mut P,
    ctx: &PrewarmContext<'_>,
) -> anyhow::Result<PrewarmOutcome> {
    let resolved = match resolve_base(ctx.repo, ctx.config_path, ctx.config, ctx.no_cache).await {
        Some(r) => r,
        None => {
            return Ok(PrewarmOutcome::CacheMiss { base_sha: None });
        }
    };

    let base_sha = resolved.base_sha.as_str();
    let label = &resolved.kind;
    let note_config = if ctx.no_cache {
        None
    } else {
        Some(ctx.config_path)
    };

    // --- Stage 1: Cache hit -- thin diff on cached image ---
    if let Some(cached_id) = resolved.cached_image_id.as_deref() {
        eprintln!(
            "[cache] {} hit: using cached image from {}",
            label,
            &base_sha[..8.min(base_sha.len())]
        );

        match try_thin_diff(
            provider,
            ctx.repo,
            cached_id,
            base_sha,
            &ctx.config.offload.sandbox_project_root,
            ctx.discovery_done,
            ctx.tracer,
        )
        .await
        {
            Ok(image_id) => return Ok(PrewarmOutcome::Resolved { image_id }),
            Err(e) => {
                warn!(
                    "{} thin diff failed, falling back to base build: {}",
                    label, e
                );
                eprintln!(
                    "[cache] {} thin diff failed, falling back to base build",
                    label
                );
            }
        }
    }

    // --- Stage 2: Base build -- export tree, build base, write note, thin diff ---
    if ctx.no_cache {
        eprintln!(
            "[prepare] --no-cache with {}: building from {} (no cache lookup)",
            label,
            &base_sha[..8.min(base_sha.len())]
        );
    } else {
        eprintln!(
            "[cache] {} miss: no cached image for {} -- building base",
            label,
            &base_sha[..8.min(base_sha.len())]
        );
    }

    // Export base tree
    let tree_dir = tempfile::tempdir().context("failed to create temp dir for base tree")?;
    git::export_tree(ctx.repo, base_sha, tree_dir.path())
        .await
        .with_context(|| format!("failed to export tree for {}", base_sha))?;

    // Build base image from exported tree
    eprintln!("[prepare] Building base image...");
    let base_image_id = {
        let _span = ctx.tracer.span(
            "checkpoint_base_prepare",
            "local",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        with_retry!(provider.prepare(
            ctx.copy_dir_tuples,
            ctx.no_cache,
            ctx.config.offload.sandbox_init_cmd.as_deref(),
            None,
            Some(tree_dir.path()),
        ))
        .context("Failed to build base image")?
    };

    // Write note if caching enabled
    if let (Some(cfg_path), Some(base_id)) = (note_config, &base_image_id) {
        write_note_for_commit(ctx.repo, base_sha, base_id, cfg_path).await;
    }

    // Build thin diff on top of base
    let Some(base_id) = base_image_id else {
        return Ok(PrewarmOutcome::CacheMiss {
            base_sha: Some(base_sha.to_string()),
        });
    };

    match try_thin_diff(
        provider,
        ctx.repo,
        &base_id,
        base_sha,
        &ctx.config.offload.sandbox_project_root,
        ctx.discovery_done,
        ctx.tracer,
    )
    .await
    {
        Ok(image_id) => Ok(PrewarmOutcome::Resolved { image_id }),
        Err(e) => {
            warn!(
                "Thin diff failed after base build, falling back to full build: {}",
                e
            );
            eprintln!("[cache] Thin diff failed, falling back to full build");
            Ok(PrewarmOutcome::CacheMiss {
                base_sha: Some(base_sha.to_string()),
            })
        }
    }
}

/// Generate a checkpoint diff and build a thin-diff image via the provider.
///
/// Returns the final image ID on success, or a `ProviderError` on failure.
async fn try_thin_diff<P: SandboxProvider>(
    provider: &mut P,
    repo: &Path,
    base_image_id: &str,
    checkpoint_sha: &str,
    sandbox_project_root: &str,
    discovery_done: &AtomicBool,
    tracer: &Tracer,
) -> Result<String, crate::provider::ProviderError> {
    let _span = tracer.span(
        "thin_diff",
        "local",
        crate::trace::PID_LOCAL,
        crate::trace::TID_MAIN,
    );

    // Generate diff on the Rust side
    let patch_file = git::generate_checkpoint_diff(repo, checkpoint_sha)
        .await
        .map_err(|e| {
            crate::provider::ProviderError::ExecFailed(format!(
                "failed to generate checkpoint diff: {e}"
            ))
        })?;

    // If no changes, reuse the base image directly
    let patch_file = match patch_file {
        Some(f) => f,
        None => {
            // Wait for discovery to finish before printing
            while !discovery_done.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            eprintln!("[prepare] No changes since checkpoint, reusing image");
            return Ok(base_image_id.to_string());
        }
    };

    eprintln!("[prepare] Building thin diff image...");

    // Route through provider's prepare_from_checkpoint
    match provider
        .prepare_from_checkpoint(
            base_image_id,
            patch_file.path(),
            sandbox_project_root,
            Some(discovery_done),
        )
        .await?
    {
        Some(image_id) => Ok(image_id),
        None => Err(crate::provider::ProviderError::ExecFailed(
            "prepare_from_checkpoint returned no image ID".to_string(),
        )),
    }
}

/// Show checkpoint/cache status for the current HEAD.
pub async fn status_handler(repo: &Path, config_path: &str, remote: &str) -> anyhow::Result<()> {
    let path = Path::new(config_path);
    let config = crate::config::load_config(path)
        .with_context(|| format!("Failed to load config from {}", config_path))?;

    // Best-effort fetch and configure notes
    let _ = git::fetch_notes(repo, remote).await;
    let _ = git::configure_notes_fetch(repo, remote).await;

    let head = git::head_sha(repo)
        .await
        .context("Failed to get HEAD SHA")?;
    let short_head = &head[..8.min(head.len())];

    if let Some(ref checkpoint_cfg) = config.checkpoint {
        let info = resolve_checkpoint(repo, config_path, checkpoint_cfg, 100)
            .await
            .context("Failed to resolve checkpoint")?;

        let Some(info) = info else {
            println!("HEAD:               {}", short_head);
            println!("Base commit:        (no checkpoint found in last 100 commits)");
            println!("Next run mode:      full build (no checkpoint found)");
            return Ok(());
        };

        let short_base = &info.checkpoint_sha[..8.min(info.checkpoint_sha.len())];

        let distance = match git::ancestors(repo, 100).await {
            Ok(ancestors) => ancestors.iter().position(|sha| sha == &info.checkpoint_sha),
            Err(_) => None,
        };
        let distance_label = match distance {
            Some(d) => format!("{} commits back", d),
            None => "unknown distance".to_string(),
        };

        match info.cached_image {
            Some(cached) => {
                let run_mode = if info.checkpoint_sha == head {
                    "use checkpoint image directly (HEAD is the checkpoint)".to_string()
                } else {
                    match git::diff_file_count(repo, &info.checkpoint_sha, &head).await {
                        Ok(count) => {
                            format!("thin diff ({} files changed since checkpoint)", count)
                        }
                        Err(_) => "thin diff".to_string(),
                    }
                };

                println!("HEAD:               {}", short_head);
                println!(
                    "Base commit:        {} (checkpoint, {})",
                    short_base, distance_label
                );
                println!("Cached image:       {}", cached.image_id);
                println!("Next run mode:      {}", run_mode);
            }
            None => {
                println!("HEAD:               {}", short_head);
                println!(
                    "Base commit:        {} (checkpoint, {})",
                    short_base, distance_label
                );
                println!("Cached image:       (none)");
                println!("Next run mode:      full build (no cached checkpoint image)");
            }
        }
    } else {
        let info = resolve_latest_commit(repo, config_path)
            .await
            .context("Failed to resolve latest commit")?;

        let Some(info) = info else {
            println!("HEAD:               (none -- empty repo)");
            println!("Next run mode:      full build");
            return Ok(());
        };

        let short_base = &info.head_sha[..8.min(info.head_sha.len())];

        match info.cached_image {
            Some(cached) => {
                println!("HEAD:               {}", short_head);
                println!("Base commit:        {} (latest commit, HEAD)", short_base);
                println!("Cached image:       {}", cached.image_id);
                println!("Next run mode:      thin diff (uncommitted changes only)");
            }
            None => {
                println!("HEAD:               {}", short_head);
                println!("Base commit:        {} (latest commit, HEAD)", short_base);
                println!("Cached image:       (none)");
                println!("Next run mode:      full build (no cached image for HEAD)");
            }
        }
    }

    Ok(())
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
