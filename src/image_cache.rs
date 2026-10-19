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
//! A cached base image is keyed by the base commit and the [`Recipe`] stored in
//! its note entry: lookup takes the entry whose stored recipe equals the current
//! one, so a config edit that shapes the image misses and configs that differ
//! only in fields the builder never sees share one image.  Cached image IDs are
//! stored as git notes (`refs/notes/offload-images`) so they travel with the
//! repo and are shared across machines.

pub mod recipe;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::config::schema::CheckpointConfig;
use crate::git;
use crate::provider::{PrepareContext, ProviderError, ProviderResult};
use crate::trace::Tracer;
use crate::with_retry;
use recipe::Recipe;

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
/// Skips the git-note lookup when `no_cache` is true; otherwise the lookup
/// matches the note's entries against `recipe`.
pub async fn resolve_latest_commit(
    repo: &Path,
    recipe: &Recipe,
    no_cache: bool,
) -> Result<Option<LatestCommitInfo>> {
    let head_sha = match git::head_sha(repo).await {
        Ok(sha) => sha,
        Err(_) => return Ok(None),
    };

    let cached_image = if no_cache {
        None
    } else {
        read_cached_image_for_commit(repo, &head_sha, recipe).await?
    };

    Ok(Some(LatestCommitInfo {
        head_sha,
        cached_image,
    }))
}

/// Checkpoint mode: find the nearest ancestor that touches any `build_inputs` file.
///
/// Returns `None` if no such ancestor exists.
/// Skips the git-note lookup when `no_cache` is true; otherwise the lookup
/// matches the note's entries against `recipe`.
pub async fn resolve_checkpoint(
    repo: &Path,
    recipe: &Recipe,
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
        read_cached_image_for_commit(repo, &checkpoint_sha, recipe).await?
    };

    Ok(Some(CheckpointInfo {
        checkpoint_sha,
        cached_image,
    }))
}

/// Look up the entry on `commit_sha` whose stored recipe equals `recipe` as JSON.
///
/// Exact equality is the whole test, so an entry without a recipe (written by an
/// older Offload) or with a shape this version does not know never matches; a
/// miss against an existing note prints one line saying what the note holds.
async fn read_cached_image_for_commit(
    repo: &Path,
    commit_sha: &str,
    recipe: &Recipe,
) -> Result<Option<CachedImage>> {
    let Some(verdicts) = classify_note(repo, commit_sha, recipe).await? else {
        return Ok(None);
    };

    if let Some(image_id) = matching_image(&verdicts) {
        return Ok(Some(CachedImage {
            image_id: image_id.to_string(),
        }));
    }

    let legacy = verdicts
        .iter()
        .filter(|entry| entry.verdict == Verdict::Legacy)
        .count();
    let different = verdicts
        .iter()
        .filter(|entry| matches!(entry.verdict, Verdict::Differs(_)))
        .count();
    eprintln!(
        "[cache] No entry on {} matches the current recipe: {} written by an older Offload (no recipe), {} with a different recipe",
        &commit_sha[..8.min(commit_sha.len())],
        legacy,
        different
    );
    Ok(None)
}

/// Why a note entry does or does not serve the current recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
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
struct EntryVerdict {
    name: String,
    image_id: String,
    verdict: Verdict,
}

/// Read the note on `commit_sha` and classify its entries against `recipe`; `None` when the
/// commit carries no note.
async fn classify_note(
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
fn matching_image(verdicts: &[EntryVerdict]) -> Option<&str> {
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
fn entry_line(entry: &EntryVerdict) -> String {
    let state = match &entry.verdict {
        Verdict::Matches => "matches".to_string(),
        Verdict::Differs(paths) => format!("differs: {}", paths.join(", ")),
        Verdict::Legacy => "legacy (written by an older Offload)".to_string(),
    };
    format!("  {} {} {}", entry.name, entry.image_id, state)
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
    /// The recipe the lookup used, so a base built after a miss is written under the same key.
    pub recipe: Recipe,
}

/// The tree a base-image build reads from, so a cached image depends only on the checkpoint
/// export (or the fallback's snapshot) and never on the live working tree.
pub(crate) struct BuildContext<'a> {
    /// Directory the image is built from: an export of the checkpoint tree, or a snapshot of
    /// the invoking directory.
    pub dir: &'a Path,
    /// The invoking directory relative to `root`; empty when `dir` is a snapshot of the invoking
    /// directory itself.
    pub cwd_rel: &'a Path,
    /// The on-disk directory `dir` mirrors (the repository root for an export, the invoking
    /// directory for a snapshot), so absolute sources under it map to their copy in `dir`.
    pub root: &'a Path,
}

impl BuildContext<'_> {
    /// The context's copy of the invoking directory, which `include_cwd` adds to the image as
    /// `/app`.
    pub(crate) fn include_dir(&self) -> PathBuf {
        normalized(self.dir.join(self.cwd_rel))
    }

    /// Where the builder reads a copy source: relative sources from the invoking directory's
    /// copy, absolute sources under `root` from theirs, and anything else as given because the
    /// context holds no counterpart.
    pub(crate) fn resolve_source(&self, source: &Path) -> PathBuf {
        if !source.is_absolute() {
            return normalized(self.include_dir().join(source));
        }
        match source.strip_prefix(self.root) {
            Ok(inside) => normalized(self.dir.join(inside)),
            Err(_) => source.to_path_buf(),
        }
    }

    /// `source:target` copy specs with every source resolved, config entries first and runtime
    /// `--copy-dir` pairs second, which is the order the recipe records.
    pub(crate) fn copy_specs(
        &self,
        config_copy_dirs: &[String],
        runtime_copy_dirs: &[(PathBuf, PathBuf)],
    ) -> Vec<String> {
        let config = config_copy_dirs
            .iter()
            .map(|spec| match spec.split_once(':') {
                Some((source, target)) => {
                    format!(
                        "{}:{}",
                        self.resolve_source(Path::new(source)).display(),
                        target
                    )
                }
                None => spec.clone(),
            });
        let runtime = runtime_copy_dirs.iter().map(|(source, target)| {
            format!(
                "{}:{}",
                self.resolve_source(source).display(),
                target.display()
            )
        });
        config.chain(runtime).collect()
    }
}

/// Drop the `.` components and trailing separators that `Path::join` leaves in place.
fn normalized(path: PathBuf) -> PathBuf {
    path.components().collect()
}

/// Provider-specific image build operations.
#[async_trait::async_trait]
pub(crate) trait ImageBuilder: Send {
    /// Build the base image, reading every file from `build_ctx` rather than the working
    /// tree: a cached image must depend only on the checkpoint export it was built from.
    async fn build_full(
        &mut self,
        copy_dirs: &[(PathBuf, PathBuf)],
        sandbox_init_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
        build_ctx: &BuildContext<'_>,
    ) -> ProviderResult<Option<String>>;

    /// Build a thin-diff image on top of a base image.
    async fn build_incremental(
        &mut self,
        base_image_id: &str,
        patch_file: Option<&Path>,
        sandbox_project_root: &str,
        post_patch_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
    ) -> ProviderResult<Option<String>>;
}

/// Result of [`run_prewarm_pipeline`].
pub(crate) enum PrewarmOutcome {
    /// A usable image was produced (cache hit + thin diff, or base build + thin diff).
    Resolved { image_id: String },
    /// No image produced — caller should fall back to a full build.
    CacheMiss,
}

/// Pick the base commit and (optionally) its cached image before provider dispatch.
///
/// Selects checkpoint or latest-commit mode based on the config, then returns
/// the base SHA, any cached image ID, and the recipe the lookup used.  Best-effort:
/// returns `None` on failure, outside a git repo, when no base commit can be
/// identified, or when the provider builds no base image to cache.
pub async fn resolve_base(
    repo: &Path,
    config: &Config,
    copy_dirs: &[(PathBuf, PathBuf)],
    no_cache: bool,
) -> Option<ResolvedBase> {
    if git::repo_root(repo).await.is_err() {
        info!("Not in a git repo, skipping checkpoint/cache resolution");
        return None;
    }

    let Some(recipe) = Recipe::from_config(config, copy_dirs) else {
        info!("Provider builds no base image, skipping checkpoint/cache resolution");
        return None;
    };

    if !no_cache {
        if let Err(e) = git::fetch_notes(repo, "origin").await {
            warn!("Failed to fetch notes: {}", e);
        }
        if let Err(e) = git::configure_notes_fetch(repo, "origin").await {
            warn!("Failed to configure notes fetch: {}", e);
        }
    }

    // Resolve (base_sha, cached_image, kind) — one call per mode.
    let result: Result<Option<(String, Option<CachedImage>, BaseKind)>> =
        if let Some(checkpoint_cfg) = config.checkpoint.as_ref() {
            resolve_checkpoint(repo, &recipe, checkpoint_cfg, no_cache)
                .await
                .map(|opt| {
                    opt.map(|info| (info.checkpoint_sha, info.cached_image, BaseKind::Checkpoint))
                })
        } else {
            resolve_latest_commit(repo, &recipe, no_cache)
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
            recipe,
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

/// The note entry for a freshly built base image and the recipe name it is stored under.  The
/// config path is provenance only, so one that cannot be made repo-relative is left out rather
/// than blocking the write.
async fn recipe_note_entry(
    repo: &Path,
    image_id: &str,
    recipe: &Recipe,
    config_path: &Path,
) -> Result<(String, git::ImageEntry)> {
    let repo_root = git::repo_root(repo)
        .await
        .context("failed to get repo root for note")?;
    let config_path =
        git::canonicalize_config_path(&config_path.to_string_lossy(), &repo_root).ok();
    let entry = git::ImageEntry {
        image_id: image_id.to_string(),
        recipe: Some(serde_json::to_value(recipe).context("failed to serialize recipe")?),
        config_path,
        offload_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        built_at: Some(humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()),
    };
    Ok((recipe.name()?, entry))
}

/// Persist a cached image ID as a git note on `commit_sha` under the recipe's name and
/// push (best-effort).  The recipe is stored inline so lookup can compare it and
/// `git notes show` can explain a miss; the config path, Offload version, and build
/// time are provenance that nothing reads.
pub async fn write_note_for_commit(
    repo: &Path,
    commit_sha: &str,
    image_id: &str,
    recipe: &Recipe,
    config_path: &Path,
) {
    let (name, entry) = match recipe_note_entry(repo, image_id, recipe, config_path).await {
        Ok(named) => named,
        Err(e) => {
            warn!("Failed to build image cache note entry: {:#}", e);
            return;
        }
    };

    let mut contents = git::NoteContents::new();
    contents.insert(name, entry);

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
    let resolved = match resolve_base(ctx.repo, ctx.config, ctx.copy_dirs, ctx.no_cache).await {
        Some(r) => r,
        None => {
            return Ok(PrewarmOutcome::CacheMiss);
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
            ctx.post_patch_cmd,
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
    let repo_root = git::repo_root(ctx.repo).await?;
    let cwd_rel = git::path_within_repo(ctx.repo).await?;
    let build_ctx = BuildContext {
        dir: tree_dir.path(),
        cwd_rel: &cwd_rel,
        root: &repo_root,
    };

    // Build base image from exported tree
    let base_image_id = {
        let _progress = crate::timing::progress_span("prepare", "base image build");
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
            &build_ctx,
        ))
        .context("Failed to build base image")?
    };

    // Write note if caching enabled
    if let (Some(cfg_path), Some(base_id)) = (note_config, &base_image_id) {
        write_note_for_commit(ctx.repo, base_sha, base_id, &resolved.recipe, cfg_path).await;
    }

    // Build thin diff on top of base
    let Some(base_id) = base_image_id else {
        return Ok(PrewarmOutcome::CacheMiss);
    };

    match try_thin_diff(
        builder,
        ctx.repo,
        &base_id,
        base_sha,
        patch_root,
        ctx.post_patch_cmd,
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
            Ok(PrewarmOutcome::CacheMiss)
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
        Ok(PrewarmOutcome::CacheMiss) => full_build_fallback(builder, ctx).await,
        Err(e) => {
            warn!("Prewarm pipeline failed: {}", e);
            full_build_fallback(builder, ctx).await
        }
    }
}

/// Full-build fallback: snapshot working directory and build from scratch.
///
/// The resulting image's filesystem matches the working tree, NOT any
/// committed tree, so we deliberately do not write a `for_commit(...)`
/// cache note here -- doing so would poison the cache for future runs
/// that expect the note's image to match the checkpoint commit's tree
/// (see `try_thin_diff` and Stage 2 of `run_prewarm_pipeline`). When
/// prewarm Stage 2 wrote a note for a freshly-built base image before
/// falling back, leaving it in place lets future runs reuse that
/// correct base.
///
/// Shared between `DefaultProvider` and `ModalProvider`.
pub(crate) async fn full_build_fallback<B: ImageBuilder>(
    builder: &mut B,
    ctx: &PrepareContext<'_>,
) -> ProviderResult<Option<String>> {
    let context_snapshot = snapshot_working_directory(ctx.tracer).map_err(|e| {
        ProviderError::ExecFailed(format!("failed to snapshot working directory: {e}"))
    })?;
    let invoking_dir = std::env::current_dir()
        .map_err(|e| ProviderError::ExecFailed(format!("failed to get current directory: {e}")))?;
    let build_ctx = BuildContext {
        dir: context_snapshot.path(),
        cwd_rel: Path::new(""),
        root: &invoking_dir,
    };

    let image_id = {
        let _progress = crate::timing::progress_span("prepare", "full image build");
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
            &build_ctx,
        ))
        .map_err(|e| ProviderError::ExecFailed(format!("Failed to prepare provider: {e}")))?
    };

    // If post_patch_cmd is configured, run it via build_incremental on the
    // freshly-built image so derived artifacts are regenerated even in the
    // fallback path.
    let image_id = match (image_id, ctx.post_patch_cmd) {
        (Some(base_id), Some(_)) => {
            let patch_root = ctx
                .config
                .offload
                .sandbox_repo_root
                .as_deref()
                .unwrap_or("/app");
            let _progress = crate::timing::progress_span("prepare", "post-patch command");
            builder
                .build_incremental(
                    &base_id,
                    None,
                    patch_root,
                    ctx.post_patch_cmd,
                    Some(ctx.discovery_done),
                )
                .await?
        }
        (id, _) => id,
    };

    Ok(image_id)
}

/// Apply the diff between `checkpoint_sha` and the working tree on top of
/// `base_image_id` to produce a new image.  Returns the base image unchanged
/// when there is no diff and no `post_patch_cmd`.
#[allow(clippy::too_many_arguments)]
async fn try_thin_diff<B: ImageBuilder>(
    builder: &mut B,
    repo: &Path,
    base_image_id: &str,
    checkpoint_sha: &str,
    sandbox_project_root: &str,
    post_patch_cmd: Option<&str>,
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

    let _progress = match (&patch_file, post_patch_cmd) {
        (None, None) => {
            // No diff, no post_patch_cmd — reuse base image
            while !discovery_done.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            eprintln!("[prepare] No changes since checkpoint, reusing image");
            return Ok(base_image_id.to_string());
        }
        (None, Some(_)) => crate::timing::progress_span("prepare", "post-patch command"),
        (Some(_), _) => crate::timing::progress_span("prepare", "thin diff image build"),
    };

    // Route through builder's incremental build
    match builder
        .build_incremental(
            base_image_id,
            patch_file.as_ref().map(|f| f.path()),
            sandbox_project_root,
            post_patch_cmd,
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

/// Print a human-readable summary of the cache state for the current HEAD, ending with one
/// line per note entry on the base commit saying why it does or does not match.
pub async fn status_handler(repo: &Path, config_path: &str, remote: &str) -> anyhow::Result<()> {
    let path = Path::new(config_path);
    let config = crate::config::load_config(path)
        .with_context(|| format!("Failed to load config from {}", config_path))?;

    // The recipe `offload build` would look up: no runtime copy dirs.
    let Some(recipe) = Recipe::from_config(&config, &[]) else {
        println!("Provider caches no images; nothing to look up.");
        return Ok(());
    };

    // Best-effort fetch and configure notes
    let _ = git::fetch_notes(repo, remote).await;
    let _ = git::configure_notes_fetch(repo, remote).await;

    let head = git::head_sha(repo)
        .await
        .context("Failed to get HEAD SHA")?;
    let short_head = &head[..8.min(head.len())];

    // The resolvers only pick the base commit here; its note is classified below so every
    // entry is explained instead of reduced to a hit or a miss.
    let verdicts = if let Some(ref checkpoint_cfg) = config.checkpoint {
        let info = resolve_checkpoint(repo, &recipe, checkpoint_cfg, true)
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

        let verdicts = classify_note(repo, &info.checkpoint_sha, &recipe).await?;
        match verdicts.as_deref().and_then(matching_image) {
            Some(image_id) => {
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
                println!("Cached image:       {}", image_id);
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
        verdicts
    } else {
        let info = resolve_latest_commit(repo, &recipe, true)
            .await
            .context("Failed to resolve latest commit")?;

        let Some(info) = info else {
            println!("HEAD:               (none -- empty repo)");
            println!("Next run mode:      full build");
            return Ok(());
        };

        let short_base = &info.head_sha[..8.min(info.head_sha.len())];

        let verdicts = classify_note(repo, &info.head_sha, &recipe).await?;
        match verdicts.as_deref().and_then(matching_image) {
            Some(image_id) => {
                println!("HEAD:               {}", short_head);
                println!("Base commit:        {} (latest commit, HEAD)", short_base);
                println!("Cached image:       {}", image_id);
                println!("Next run mode:      thin diff (uncommitted changes only)");
            }
            None => {
                println!("HEAD:               {}", short_head);
                println!("Base commit:        {} (latest commit, HEAD)", short_base);
                println!("Cached image:       (none)");
                println!("Next run mode:      full build (no cached image for HEAD)");
            }
        }
        verdicts
    };

    println!("Recipe:             {}", recipe.name()?);
    let entries = verdicts.as_deref().unwrap_or_default();
    if entries.is_empty() {
        println!("  (no note on this commit)");
    }
    for entry in entries {
        println!("{}", entry_line(entry));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DefaultFrameworkConfig, FrameworkConfig, ModalProviderConfig, OffloadConfig, ProviderConfig,
    };
    use crate::git::{ImageEntry, NOTES_REF, NoteContents};

    use std::path::Path;

    use anyhow::bail;
    use serde_json::json;

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

    /// Helper: a minimal Modal config; tests change the one field they vary.
    fn minimal_modal_config() -> Config {
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

    /// Helper: the recipe `offload build` derives from `config` (no runtime copy dirs).
    fn recipe_for(config: &Config) -> Result<Recipe> {
        Recipe::from_config(config, &[]).context("the Modal provider should yield a recipe")
    }

    /// Helper: write an entry for `recipe` on `sha` the way a current Offload does.
    fn write_recipe_entry_in(dir: &Path, sha: &str, recipe: &Recipe, image_id: &str) -> Result<()> {
        let entry = ImageEntry {
            recipe: Some(serde_json::to_value(recipe)?),
            ..ImageEntry::legacy(image_id)
        };
        let mut contents = NoteContents::new();
        contents.insert(recipe.name()?, entry);
        write_note_in(dir, sha, &contents)
    }

    /// Helper: the verdict for one note entry.
    fn entry_verdict(name: &str, image_id: &str, verdict: Verdict) -> EntryVerdict {
        EntryVerdict {
            name: name.to_string(),
            image_id: image_id.to_string(),
            verdict,
        }
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

        let recipe = recipe_for(&minimal_modal_config())?;
        let result = resolve_checkpoint(dir.path(), &recipe, &cfg, false).await?;

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

        let recipe = recipe_for(&minimal_modal_config())?;
        let result = resolve_checkpoint(dir.path(), &recipe, &cfg, false).await?;

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

        // Write a recipe entry on the checkpoint commit
        let config = minimal_modal_config();
        write_recipe_entry_in(
            dir.path(),
            &checkpoint_sha,
            &recipe_for(&config)?,
            "im-cached123",
        )?;

        // Add another commit that doesn't touch Dockerfile
        std::fs::write(dir.path().join("app.py"), "print('hello')")?;
        git_cmd(dir.path(), &["add", "app.py"])?;
        git_cmd(dir.path(), &["commit", "-m", "add app"])?;

        let cfg = CheckpointConfig {
            build_inputs: vec!["Dockerfile".to_string()],
        };

        let result = resolve_checkpoint(dir.path(), &recipe_for(&config)?, &cfg, false).await?;

        let info = result.context("should find checkpoint")?;
        assert_eq!(info.checkpoint_sha, checkpoint_sha);
        let cached = info.cached_image.context("should have cached image")?;
        assert_eq!(cached.image_id, "im-cached123");
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_checkpoint_misses_when_sandbox_init_cmd_changes() -> Result<()> {
        let dir = init_temp_repo()?;

        std::fs::write(dir.path().join("Dockerfile"), "FROM ubuntu:22.04")?;
        git_cmd(dir.path(), &["add", "Dockerfile"])?;
        git_cmd(dir.path(), &["commit", "-m", "add Dockerfile"])?;
        let checkpoint_sha = head_sha_in(dir.path())?;

        let config = minimal_modal_config();
        write_recipe_entry_in(
            dir.path(),
            &checkpoint_sha,
            &recipe_for(&config)?,
            "im-old-init",
        )?;

        let mut changed = minimal_modal_config();
        changed.offload.sandbox_init_cmd = Some("bash other.sh".to_string());
        let cfg = CheckpointConfig {
            build_inputs: vec!["Dockerfile".to_string()],
        };

        let result = resolve_checkpoint(dir.path(), &recipe_for(&changed)?, &cfg, false).await?;

        let info = result.context("should find checkpoint")?;
        assert_eq!(info.checkpoint_sha, checkpoint_sha);
        assert!(
            info.cached_image.is_none(),
            "a changed sandbox_init_cmd must not reuse the image"
        );
        Ok(())
    }

    // ---- Tests for resolve_latest_commit ----

    #[tokio::test]
    async fn test_resolve_latest_commit_hit() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;

        // Write a recipe entry on HEAD
        let config = minimal_modal_config();
        write_recipe_entry_in(dir.path(), &head, &recipe_for(&config)?, "im-head-cached")?;

        let result = resolve_latest_commit(dir.path(), &recipe_for(&config)?, false).await?;

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
        let recipe = recipe_for(&minimal_modal_config())?;
        let result = resolve_latest_commit(dir.path(), &recipe, false).await?;

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

        let recipe = recipe_for(&minimal_modal_config())?;
        let result = resolve_latest_commit(dir.path(), &recipe, false).await?;

        assert!(result.is_none(), "empty repo has no HEAD");
        Ok(())
    }

    #[tokio::test]
    async fn test_configs_differing_only_in_report_share_one_entry() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;

        let config = minimal_modal_config();
        let mut other = minimal_modal_config();
        other.report.output_dir = PathBuf::from("other-results");
        let recipe = recipe_for(&config)?;
        let other_recipe = recipe_for(&other)?;
        assert_eq!(recipe, other_recipe);

        write_recipe_entry_in(dir.path(), &head, &recipe, "im-shared")?;

        for lookup in [&recipe, &other_recipe] {
            let info = resolve_latest_commit(dir.path(), lookup, false)
                .await?
                .context("should find latest commit")?;
            let cached = info.cached_image.context("should have cached image")?;
            assert_eq!(cached.image_id, "im-shared");
        }
        let note = read_note_in(dir.path(), &head)?.context("note should exist")?;
        assert_eq!(note.len(), 1, "one recipe means one entry");
        Ok(())
    }

    #[tokio::test]
    async fn test_stored_recipe_with_extra_key_does_not_match() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;

        let recipe = recipe_for(&minimal_modal_config())?;
        let mut stored = serde_json::to_value(&recipe)?;
        stored
            .as_object_mut()
            .context("recipe serializes to an object")?
            .insert("newer_field".to_string(), serde_json::Value::Bool(true));
        let mut contents = NoteContents::new();
        contents.insert(
            recipe.name()?,
            ImageEntry {
                recipe: Some(stored),
                ..ImageEntry::legacy("im-newer")
            },
        );
        write_note_in(dir.path(), &head, &contents)?;

        let info = resolve_latest_commit(dir.path(), &recipe, false)
            .await?
            .context("should find latest commit")?;
        assert!(
            info.cached_image.is_none(),
            "an entry with a field this version does not know must not match"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_legacy_only_note_misses() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;

        let mut contents = NoteContents::new();
        contents.insert("offload.toml".to_string(), ImageEntry::legacy("im-legacy"));
        write_note_in(dir.path(), &head, &contents)?;

        let recipe = recipe_for(&minimal_modal_config())?;
        let info = resolve_latest_commit(dir.path(), &recipe, false)
            .await?
            .context("should find latest commit")?;
        assert!(
            info.cached_image.is_none(),
            "entries without a recipe never match"
        );
        Ok(())
    }

    // ---- Tests for entry classification ----

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

    // ---- Tests for write_note_for_commit ----

    #[tokio::test]
    async fn test_write_note_for_commit_writes_entry_under_recipe_name() -> Result<()> {
        let dir = init_temp_repo()?;
        let head = head_sha_in(dir.path())?;
        let mut legacy = NoteContents::new();
        legacy.insert("offload.toml".to_string(), ImageEntry::legacy("im-legacy"));
        write_note_in(dir.path(), &head, &legacy)?;

        let recipe = recipe_for(&minimal_modal_config())?;
        write_note_for_commit(
            dir.path(),
            &head,
            "im-built",
            &recipe,
            Path::new("./offload.toml"),
        )
        .await;

        let note = read_note_in(dir.path(), &head)?.context("note should exist")?;
        assert_eq!(note.len(), 2, "the legacy entry is kept beside the new one");
        let entry = note
            .get(&recipe.name()?)
            .context("entry under the recipe name")?;
        assert_eq!(entry.image_id, "im-built");
        assert_eq!(entry.recipe, Some(serde_json::to_value(&recipe)?));
        assert_eq!(entry.config_path.as_deref(), Some("offload.toml"));
        assert_eq!(
            entry.offload_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        humantime::parse_rfc3339(
            entry
                .built_at
                .as_deref()
                .context("built_at should be set")?,
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_recipe_note_entry_omits_a_config_path_outside_the_repo() -> Result<()> {
        let dir = init_temp_repo()?;
        let recipe = recipe_for(&minimal_modal_config())?;

        let (name, entry) = recipe_note_entry(
            dir.path(),
            "im-built",
            &recipe,
            Path::new("/outside-the-repo/offload.toml"),
        )
        .await?;

        assert_eq!(name, recipe.name()?);
        assert_eq!(entry.image_id, "im-built");
        assert_eq!(entry.recipe, Some(serde_json::to_value(&recipe)?));
        assert_eq!(entry.config_path, None);
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

    // ---- Tests for BuildContext ----

    /// A context exported to `/ctx` from a repository at `/repo` invoked from its `sub/`.
    fn build_context() -> BuildContext<'static> {
        BuildContext {
            dir: Path::new("/ctx"),
            cwd_rel: Path::new("sub"),
            root: Path::new("/repo"),
        }
    }

    #[test]
    fn test_resolve_source_relative_is_under_the_invoking_directory_in_the_context() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source(Path::new("./data")),
            PathBuf::from("/ctx/sub/data")
        );
        assert_eq!(
            ctx.resolve_source(Path::new("data")),
            PathBuf::from("/ctx/sub/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_absolute_inside_the_repository_maps_into_the_context() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source(Path::new("/repo/other/data")),
            PathBuf::from("/ctx/other/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_absolute_outside_the_repository_is_unchanged() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source(Path::new("/elsewhere/data")),
            PathBuf::from("/elsewhere/data")
        );
        // A sibling whose name merely starts with the root's name is outside it.
        assert_eq!(
            ctx.resolve_source(Path::new("/repo-archive/data")),
            PathBuf::from("/repo-archive/data")
        );
        Ok(())
    }

    #[test]
    fn test_include_dir_is_the_context_at_the_invoking_directory() -> Result<()> {
        assert_eq!(build_context().include_dir(), PathBuf::from("/ctx/sub"));
        let snapshot = BuildContext {
            dir: Path::new("/snap"),
            cwd_rel: Path::new(""),
            root: Path::new("/repo/sub"),
        };
        assert_eq!(snapshot.include_dir(), PathBuf::from("/snap"));
        Ok(())
    }

    #[test]
    fn test_copy_specs_resolve_sources_and_keep_targets_in_builder_order() -> Result<()> {
        let config = vec!["./data:/data".to_string(), "no-colon".to_string()];
        let runtime = vec![(PathBuf::from("/repo/tests"), PathBuf::from("/app/tests"))];
        assert_eq!(
            build_context().copy_specs(&config, &runtime),
            vec!["/ctx/sub/data:/data", "no-colon", "/ctx/tests:/app/tests"]
        );
        Ok(())
    }

    // ---- Mock ImageBuilder for testing ----

    /// A mock implementation of `ImageBuilder` that records calls.
    struct MockImageBuilder {
        image_id: Option<String>,
        build_full_calls: u32,
        build_incremental_calls: u32,
    }

    impl MockImageBuilder {
        fn new() -> Self {
            Self {
                image_id: None,
                build_full_calls: 0,
                build_incremental_calls: 0,
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
            _build_ctx: &BuildContext<'_>,
        ) -> crate::provider::ProviderResult<Option<String>> {
            self.build_full_calls += 1;
            let id = format!("im-mock-{}", self.build_full_calls);
            self.image_id = Some(id.clone());
            Ok(Some(id))
        }

        async fn build_incremental(
            &mut self,
            _base_image_id: &str,
            _patch_file: Option<&Path>,
            _sandbox_project_root: &str,
            _post_patch_cmd: Option<&str>,
            _discovery_done: Option<&AtomicBool>,
        ) -> crate::provider::ProviderResult<Option<String>> {
            self.build_incremental_calls += 1;
            let id = format!("im-mock-incremental-{}", self.build_incremental_calls);
            self.image_id = Some(id.clone());
            Ok(Some(id))
        }
    }

    #[test]
    fn test_mock_image_builder_initial_state() {
        let builder = MockImageBuilder::new();
        assert!(builder.image_id.is_none());
        assert_eq!(builder.build_full_calls, 0);
        assert_eq!(builder.build_incremental_calls, 0);
    }

    #[tokio::test]
    async fn test_mock_image_builder_build_full() {
        let mut builder = MockImageBuilder::new();
        let result = builder.build_full(&[], None, None, &build_context()).await;
        assert!(result.is_ok());
        assert_eq!(
            result.as_ref().ok().and_then(|r| r.as_deref()),
            Some("im-mock-1")
        );
        assert_eq!(builder.build_full_calls, 1);
        assert_eq!(builder.image_id.as_deref(), Some("im-mock-1"));
    }

    #[tokio::test]
    async fn test_mock_image_builder_build_incremental_with_post_patch_cmd() {
        let mut builder = MockImageBuilder::new();
        let discovery_done = AtomicBool::new(true);
        let result = builder
            .build_incremental(
                "im-base-123",
                None,
                "/app",
                Some("scripts/regen-clients.sh"),
                Some(&discovery_done),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(
            result.as_ref().ok().and_then(|r| r.as_deref()),
            Some("im-mock-incremental-1")
        );
        assert_eq!(builder.build_incremental_calls, 1);
        assert_eq!(builder.image_id.as_deref(), Some("im-mock-incremental-1"));
    }

    /// Verifies build_incremental is called even when patch_file is None but
    /// post_patch_cmd is Some.
    #[tokio::test]
    async fn test_mock_image_builder_build_incremental_no_patch_with_post_patch_cmd() {
        let mut builder = MockImageBuilder::new();
        let discovery_done = AtomicBool::new(true);

        // First call: no patch, with post_patch_cmd
        let result = builder
            .build_incremental(
                "im-base-456",
                None,
                "/app/project",
                Some("make generate-client"),
                Some(&discovery_done),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(builder.build_incremental_calls, 1);

        // Second call: with patch, with post_patch_cmd
        let patch_path = Path::new("/tmp/test.patch");
        let result = builder
            .build_incremental(
                "im-base-456",
                Some(patch_path),
                "/app/project",
                Some("make generate-client"),
                Some(&discovery_done),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(builder.build_incremental_calls, 2);
        assert_eq!(builder.image_id.as_deref(), Some("im-mock-incremental-2"));
    }
}
