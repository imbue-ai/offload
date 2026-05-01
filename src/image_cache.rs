//! Image caching for sandbox environments.
//!
//! Building sandbox images is expensive.  This module speeds up repeated builds
//! by identifying a **base commit** — a prior commit whose image we can build
//! and cache — then applying only a thin diff on top.
//!
//! Two caching modes:
//!
//! - **Checkpoint** (`[checkpoint]` with `build_inputs` in config): the base
//!   commit is the nearest ancestor that touches any `build_inputs` file.
//! - **Latest-commit** (no `[checkpoint]`): the base commit is HEAD itself.
//!
//! Cached image IDs are stored as git notes (`refs/notes/offload-images`) so
//! they travel with the repo and are shared across machines.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::config::schema::CheckpointConfig;
use crate::git;
use crate::provider::{PrepareContext, ProviderError, ProviderResult};
use crate::trace::Tracer;
use crate::with_retry;

/// A provider-specific opaque image identifier read from a git note.
#[derive(Debug, Clone)]
pub struct CachedImage {
    pub image_id: String,
}

/// Result of checkpoint-mode resolution: the base commit and its cached image (if any).
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub checkpoint_sha: String,
    pub cached_image: Option<CachedImage>,
}

/// Result of latest-commit-mode resolution: HEAD and its cached image (if any).
#[derive(Debug, Clone)]
pub struct LatestCommitInfo {
    pub head_sha: String,
    pub cached_image: Option<CachedImage>,
}

/// Latest-commit mode: use HEAD as the base commit.
///
/// Returns `None` for an empty repo (no commits).
/// Skips the git-note lookup when `no_cache` is true.
pub async fn resolve_latest_commit(
    repo: &Path,
    config_path: &str,
    no_cache: bool,
) -> Result<Option<LatestCommitInfo>> {
    let head_sha = match git::head_sha(repo).await {
        Ok(sha) => sha,
        Err(_) => return Ok(None),
    };

    let cached_image = if no_cache {
        None
    } else {
        let repo_root = git::repo_root(repo).await?;
        let config_key = git::canonicalize_config_path(config_path, &repo_root)?;
        read_cached_image_for_commit(repo, &head_sha, &config_key).await?
    };

    Ok(Some(LatestCommitInfo {
        head_sha,
        cached_image,
    }))
}

/// Checkpoint mode: find the nearest ancestor that touches any `build_inputs` file.
///
/// Returns `None` if no such ancestor exists.
/// Skips the git-note lookup when `no_cache` is true.
pub async fn resolve_checkpoint(
    repo: &Path,
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    no_cache: bool,
) -> Result<Option<CheckpointInfo>> {
    let checkpoint_sha =
        match git::nearest_ancestor_touching(repo, &checkpoint_cfg.build_inputs).await? {
            Some(sha) => sha,
            None => return Ok(None),
        };

    let cached_image = if no_cache {
        None
    } else {
        let repo_root = git::repo_root(repo).await?;
        let config_key = git::canonicalize_config_path(config_path, &repo_root)?;
        read_cached_image_for_commit(repo, &checkpoint_sha, &config_key).await?
    };

    Ok(Some(CheckpointInfo {
        checkpoint_sha,
        cached_image,
    }))
}

/// Look up the cached image ID stored in a git note for `commit_sha`.
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

/// Which caching mode determined the base commit.
pub enum BaseKind {
    /// Base is the nearest ancestor touching a `build_inputs` file.
    Checkpoint,
    /// Base is HEAD (no `build_inputs` configured).
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

/// The base commit (and optional cached image) chosen before provider dispatch.
pub struct ResolvedBase {
    pub base_sha: String,
    pub cached_image_id: Option<String>,
    pub kind: BaseKind,
}

/// Provider-specific image build operations.
#[async_trait::async_trait]
pub(crate) trait ImageBuilder: Send {
    /// Build an image from scratch (full prepare).
    async fn build_full(
        &mut self,
        copy_dirs: &[(PathBuf, PathBuf)],
        sandbox_init_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
        context_dir: Option<&Path>,
    ) -> ProviderResult<Option<String>>;

    /// Build a thin-diff image on top of a base image.
    async fn build_incremental(
        &mut self,
        base_image_id: &str,
        patch_file: &Path,
        sandbox_project_root: &str,
        discovery_done: Option<&AtomicBool>,
    ) -> ProviderResult<Option<String>>;
}

/// Result of [`run_prewarm_pipeline`].
pub(crate) enum PrewarmOutcome {
    /// A usable image was produced (cache hit + thin diff, or base build + thin diff).
    Resolved { image_id: String },
    /// No image produced — caller should fall back to a full build.
    /// Carries the base SHA (if resolved) so the caller can write a cache note
    /// after building without re-resolving.
    CacheMiss { base_sha: Option<String> },
}

/// Pick the base commit and (optionally) its cached image before provider dispatch.
///
/// Selects checkpoint or latest-commit mode based on the config, then returns
/// the base SHA and any cached image ID.  Best-effort: returns `None` on
/// failure, outside a git repo, or when no base commit can be identified.
pub async fn resolve_base(
    repo: &Path,
    config_path: &Path,
    config: &Config,
    no_cache: bool,
) -> Option<ResolvedBase> {
    if git::repo_root(repo).await.is_err() {
        info!("Not in a git repo, skipping checkpoint/cache resolution");
        return None;
    }

    if !no_cache {
        if let Err(e) = git::fetch_notes(repo, "origin").await {
            warn!("Failed to fetch notes: {}", e);
        }
        if let Err(e) = git::configure_notes_fetch(repo, "origin").await {
            warn!("Failed to configure notes fetch: {}", e);
        }
    }

    let config_path_str = config_path.to_string_lossy();

    // Resolve (base_sha, cached_image, kind) — one call per mode.
    let result: Result<Option<(String, Option<CachedImage>, BaseKind)>> =
        if let Some(checkpoint_cfg) = config.checkpoint.as_ref() {
            resolve_checkpoint(repo, &config_path_str, checkpoint_cfg, no_cache)
                .await
                .map(|opt| {
                    opt.map(|info| (info.checkpoint_sha, info.cached_image, BaseKind::Checkpoint))
                })
        } else {
            resolve_latest_commit(repo, &config_path_str, no_cache)
                .await
                .map(|opt| {
                    opt.map(|info| (info.head_sha, info.cached_image, BaseKind::LatestCommit))
                })
        };

    match result {
        Ok(Some((base_sha, cached_image, kind))) => Some(ResolvedBase {
            base_sha,
            cached_image_id: cached_image.map(|c| c.image_id),
            kind,
        }),
        Ok(None) => {
            info!("No base commit found");
            None
        }
        Err(e) => {
            warn!("Base resolution failed: {}", e);
            None
        }
    }
}

/// Persist a cached image ID as a git note on `commit_sha` and push (best-effort).
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

/// Try to produce a sandbox image cheaply before falling back to a full build.
///
/// 1. **Cache hit** -- a cached base image exists: apply a thin diff on top.
/// 2. **Cache miss** -- no cached image: export the base tree, build a base
///    image via the builder, write a cache note, then apply a thin diff.
///
/// Returns [`PrewarmOutcome::Resolved`] with a usable image, or
/// [`PrewarmOutcome::CacheMiss`] if the caller must do a full build.
pub(crate) async fn run_prewarm_pipeline<B: ImageBuilder>(
    builder: &mut B,
    ctx: &PrepareContext<'_>,
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
    let patch_root = ctx
        .config
        .offload
        .sandbox_repo_root
        .as_deref()
        .context("sandbox_repo_root not set (config validation should have filled this)")?;

    // --- Stage 1: Cache hit -- thin diff on cached image ---
    if let Some(cached_id) = resolved.cached_image_id.as_deref() {
        eprintln!(
            "[cache] {} hit: using cached image from {}",
            label,
            &base_sha[..8.min(base_sha.len())]
        );

        match try_thin_diff(
            builder,
            ctx.repo,
            cached_id,
            base_sha,
            patch_root,
            ctx.discovery_done,
            ctx.tracer,
        )
        .await
        {
            Ok(image_id) => return Ok(PrewarmOutcome::Resolved { image_id }),
            Err(e) => {
                debug!(
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
        with_retry!(builder.build_full(
            ctx.copy_dirs,
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
        builder,
        ctx.repo,
        &base_id,
        base_sha,
        patch_root,
        ctx.discovery_done,
        ctx.tracer,
    )
    .await
    {
        Ok(image_id) => Ok(PrewarmOutcome::Resolved { image_id }),
        Err(e) => {
            debug!(
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

/// Run the prewarm pipeline, then fall back to a full build on miss.
pub(crate) async fn prepare_with_prewarm<B: ImageBuilder>(
    builder: &mut B,
    ctx: &PrepareContext<'_>,
) -> ProviderResult<Option<String>> {
    let prewarm = run_prewarm_pipeline(builder, ctx).await;
    match prewarm {
        Ok(PrewarmOutcome::Resolved { ref image_id }) => Ok(Some(image_id.clone())),
        Ok(PrewarmOutcome::CacheMiss { base_sha }) => {
            full_build_fallback(builder, ctx, base_sha).await
        }
        Err(e) => {
            warn!("Prewarm pipeline failed: {}", e);
            full_build_fallback(builder, ctx, None).await
        }
    }
}

/// Full-build fallback: snapshot working directory, build from scratch, write cache note.
///
/// Shared between `DefaultProvider` and `ModalProvider`.
pub(crate) async fn full_build_fallback<B: ImageBuilder>(
    builder: &mut B,
    ctx: &PrepareContext<'_>,
    base_sha: Option<String>,
) -> ProviderResult<Option<String>> {
    let context_snapshot = snapshot_working_directory(ctx.tracer).map_err(|e| {
        ProviderError::ExecFailed(format!("failed to snapshot working directory: {e}"))
    })?;

    let image_id = {
        let _span = ctx.tracer.span(
            "image_prepare",
            "local",
            crate::trace::PID_LOCAL,
            crate::trace::TID_MAIN,
        );
        with_retry!(builder.build_full(
            ctx.copy_dirs,
            ctx.sandbox_init_cmd,
            Some(ctx.discovery_done),
            Some(context_snapshot.path()),
        ))
        .map_err(|e| ProviderError::ExecFailed(format!("Failed to prepare provider: {e}")))?
    };

    // Write cache note if applicable
    if !ctx.no_cache
        && let (Some(sha), Some(id)) = (&base_sha, &image_id)
    {
        write_note_for_commit(ctx.repo, sha, id, ctx.config_path).await;
    }

    Ok(image_id)
}

/// Apply the diff between `checkpoint_sha` and the working tree on top of
/// `base_image_id` to produce a new image.  Returns the base image unchanged
/// when there is no diff.
async fn try_thin_diff<B: ImageBuilder>(
    builder: &mut B,
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

    // Route through builder's incremental build
    match builder
        .build_incremental(
            base_image_id,
            patch_file.path(),
            sandbox_project_root,
            Some(discovery_done),
        )
        .await?
    {
        Some(image_id) => Ok(image_id),
        None => Err(crate::provider::ProviderError::ExecFailed(
            "build_incremental returned no image ID".to_string(),
        )),
    }
}

/// Read `.dockerignore` patterns from the current directory.
///
/// Returns an empty vec if the file does not exist. Skips blank lines and
/// comments (lines starting with `#`).
fn read_dockerignore_patterns(cwd: &Path) -> Vec<String> {
    let path = cwd.join(".dockerignore");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

/// Copy the working directory into a temporary directory for use as build context.
///
/// This produces a frozen snapshot so that files modified by other processes
/// (e.g. `.pyc` bytecode caches) don't cause "modified during build" errors
/// when Modal uploads the context.
///
/// If a `.dockerignore` file is present, its patterns are passed as `--exclude`
/// rules to `rsync` so that large ignored trees (`.git`, `.venv`, etc.) are
/// never copied, keeping the snapshot fast.
pub(crate) fn snapshot_working_directory(tracer: &Tracer) -> Result<tempfile::TempDir> {
    let _span = tracer.span(
        "snapshot_cwd",
        "local",
        crate::trace::PID_LOCAL,
        crate::trace::TID_MAIN,
    );

    let snapshot = tempfile::tempdir()
        .context("Failed to create temporary directory for build context snapshot")?;
    let cwd = std::env::current_dir().context("Failed to get current directory")?;

    let ignore_patterns = read_dockerignore_patterns(&cwd);

    // rsync requires a trailing slash on the source to copy *contents* into dest.
    let mut src = cwd.as_os_str().to_os_string();
    src.push("/");

    let mut cmd = std::process::Command::new("rsync");
    cmd.arg("-a");
    for pattern in &ignore_patterns {
        cmd.arg(format!("--exclude={pattern}"));
    }
    cmd.arg("--");
    cmd.arg(&src);
    cmd.arg(snapshot.path());

    let status = cmd
        .status()
        .context("Failed to spawn rsync for working directory snapshot")?;
    if !status.success() {
        return Err(anyhow!("rsync failed with exit code {:?}", status.code()));
    }
    info!(
        "Snapshotted working directory into {} (excluded {} .dockerignore pattern(s))",
        snapshot.path().display(),
        ignore_patterns.len(),
    );
    Ok(snapshot)
}

/// Print a human-readable summary of the cache state for the current HEAD.
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
        let info = resolve_checkpoint(repo, config_path, checkpoint_cfg, false)
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
        let info = resolve_latest_commit(repo, config_path, false)
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

        let result = resolve_checkpoint(dir.path(), "offload.toml", &cfg, false).await?;

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

        let result = resolve_checkpoint(dir.path(), "offload.toml", &cfg, false).await?;

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

        let result = resolve_checkpoint(dir.path(), "offload.toml", &cfg, false).await?;

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

        let result = resolve_latest_commit(dir.path(), "offload.toml", false).await?;

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
        let result = resolve_latest_commit(dir.path(), "offload.toml", false).await?;

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

        let result = resolve_latest_commit(dir.path(), "offload.toml", false).await?;

        assert!(result.is_none(), "empty repo has no HEAD");
        Ok(())
    }

    // ---- Tests for snapshot_working_directory ----

    #[test]
    fn test_snapshot_working_directory_creates_copy() {
        let tracer = Tracer::noop();
        // snapshot_working_directory copies the current working directory.
        // We just verify it produces a non-empty temp dir without error.
        let snapshot = snapshot_working_directory(&tracer);
        assert!(snapshot.is_ok(), "snapshot should succeed");
        if let Ok(ref snap) = snapshot {
            let snap_path = snap.path();
            assert!(snap_path.exists(), "snapshot directory should exist");
            // The snapshot should contain at least one file (the cwd has files).
            let has_entries = std::fs::read_dir(snap_path)
                .ok()
                .map(|mut rd| rd.next().is_some())
                .unwrap_or(false);
            assert!(has_entries, "snapshot should contain files from cwd");
        }
    }

    // ---- Mock ImageBuilder for testing ----

    /// A mock implementation of `ImageBuilder` that records calls.
    struct MockImageBuilder {
        image_id: Option<String>,
        build_full_calls: u32,
    }

    impl MockImageBuilder {
        fn new() -> Self {
            Self {
                image_id: None,
                build_full_calls: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl ImageBuilder for MockImageBuilder {
        async fn build_full(
            &mut self,
            _copy_dirs: &[(PathBuf, PathBuf)],
            _sandbox_init_cmd: Option<&str>,
            _discovery_done: Option<&AtomicBool>,
            _context_dir: Option<&Path>,
        ) -> crate::provider::ProviderResult<Option<String>> {
            self.build_full_calls += 1;
            let id = format!("im-mock-{}", self.build_full_calls);
            self.image_id = Some(id.clone());
            Ok(Some(id))
        }

        async fn build_incremental(
            &mut self,
            _base_image_id: &str,
            _patch_file: &Path,
            _sandbox_project_root: &str,
            _discovery_done: Option<&AtomicBool>,
        ) -> crate::provider::ProviderResult<Option<String>> {
            let id = "im-mock-incremental".to_string();
            self.image_id = Some(id.clone());
            Ok(Some(id))
        }
    }

    #[test]
    fn test_mock_image_builder_initial_state() {
        let builder = MockImageBuilder::new();
        assert!(builder.image_id.is_none());
        assert_eq!(builder.build_full_calls, 0);
    }

    #[tokio::test]
    async fn test_mock_image_builder_build_full() {
        let mut builder = MockImageBuilder::new();
        let result = builder.build_full(&[], None, None, None).await;
        assert!(result.is_ok());
        assert_eq!(
            result.as_ref().ok().and_then(|r| r.as_deref()),
            Some("im-mock-1")
        );
        assert_eq!(builder.build_full_calls, 1);
        assert_eq!(builder.image_id.as_deref(), Some("im-mock-1"));
    }
}
