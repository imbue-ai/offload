# Checkpoint Images -- Implementation Plan

Implementation plan for the checkpoint images spec (`checkpoint-images.spec.md`).

## Key Technical Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Git operations | Shell out to `git` CLI via `spawn_blocking` | No `git2` crate (heavy native dep); works with jj colocated repos; `spawn_blocking` prevents stalling the tokio runtime |
| Notes content | JSON keyed by TOML config path | Prevents collision when multiple configs target different Dockerfiles |
| `.offload-image-cache` | Remove from `modal_sandbox.py` | Git notes are the sole caching mechanism; `.offload-image-cache` is superseded |
| Checkpoint detection | `git diff-tree --no-commit-id --name-only -r -m <sha>` | Handles merge commits (all parents); pure function of commit content |
| Config path keys | Repo-relative, no `./` prefix | Canonical form prevents duplicate entries |
| JSON in notes | Pretty-printed (indented) | Human-debuggable via `git notes show` |
| No `CheckpointProvider` trait | Checkpoint fields go directly on `SandboxProvider` | Both Modal and Default providers have identical checkpoint/image_id fields; a separate trait is unnecessary indirection for trivial field accessors |
| Provider trait extended | `prewarm_image_cache()` and `prepare_from_checkpoint()` added to `SandboxProvider` | Thin-diff image build is routed through `provider.prepare_from_checkpoint()`, and the entire prewarm pipeline is invoked via `provider.prewarm_image_cache()`. The provider stores the resulting image_id internally. |
| Diff generation in Rust | Rust generates a unified binary patch using a temporary git index (`git read-tree` + `git add -A` + `git diff --cached --binary`), capturing both tracked changes and untracked files in one patch. Passes `--patch-file` to the provider's `prepare_from_checkpoint()` | Keeps all git logic in Rust; Python is a thin SDK wrapper that only applies a pre-generated patch via Modal API calls. The temp-index approach avoids separate handling of untracked files. |
| Fallthrough caching | Cache lookups are transparent steps in a linear pipeline | No scattered if/else trees; each step either produces a value or falls through to the next |
| Unified caching pipeline | Checkpoint and LatestCommit follow identical steps after base-commit resolution | The only difference is how the base commit is selected (nearest ancestor touching `build_inputs` vs HEAD). Everything after resolution -- cache lookup, tree export, base build, thin diff, note write -- is the same code path. Variant names are kept for logging/diagnostics |
| Latest-commit base commit | Use HEAD (latest commit) as base image | Thin diff covers only uncommitted changes (smaller than diffing from HEAD~1); cache hit rate is the same (miss on new commit, hit on repeated runs); avoids initial-commit edge case since HEAD always exists. Non-checkpoint caching uses HEAD (not HEAD~1) as the base commit. |
| `--no-cache` preserves build procedure | `--no-cache` skips note interactions but still exports the base commit tree and builds from `context_dir` | Both Checkpoint and LatestCommit paths export a clean tree and pass it as `context_dir` so `COPY . /app` gets a deterministic checkout. Falling through to a plain full build uses `context_dir=None`, which copies the live CWD (includes `.git/`, untracked files, etc.) -- producing a different and likely broken image. `--no-cache` means "don't use the cache," not "use a different build procedure." |

## Design: Caching Flow

The caching flow is a **linear fallthrough pipeline**. Each step either succeeds (short-circuits) or falls through to the next.

### Base commit resolution

The first step determines the base commit. This is the **only** point where Checkpoint and LatestCommit diverge:

```
resolve_base_commit():
  if [checkpoint] config present:
    walk ancestors, find first commit touching build_inputs
    read note on that commit → ResolvedBase { kind: Checkpoint, base_sha, cached_image_id }
    (returns None if no checkpoint found in window)
  else:
    use HEAD as base commit
    read note on HEAD → ResolvedBase { kind: LatestCommit, base_sha, cached_image_id }
    (returns None if empty repo with no commits)
```

The `kind` field (Checkpoint vs LatestCommit) is used only for log messages (e.g. "[cache] Checkpoint hit" vs "[cache] Latest-commit hit").

### Unified pipeline (after resolution)

Both kinds follow identical steps:

```
Stage 1 (cache hit):
  if cached_image_id is Some(image_id):
    try_thin_diff(provider, image_id, base_sha) → return Resolved { image_id }
    on failure → fall through to Stage 2

Stage 2 (cache miss / base build):
  export_tree(base_sha)
  base_id = provider.prepare(context_dir=exported_tree)
  if !no_cache: write_note(base_sha, base_id), push_notes
  try_thin_diff(provider, base_id, base_sha) → return Resolved { image_id }
  on failure → return CacheMiss { base_sha }

No base found → return CacheMiss { base_sha: None }
  // Caller (main.rs) falls back to full build with snapshot_working_directory()
```

`--no-cache` follows the same unified pipeline but skips all note interactions (no fetch, read, write, or push). It still resolves the base SHA, exports the tree, builds from `context_dir`, and applies thin diff -- producing the same image as a normal cache miss. The only difference is that the result is not persisted to git notes.

When `no_cache` is true, `resolve_base()` skips notes fetch and returns `ResolvedBase` with `cached_image_id: None`, ensuring the cache-hit stage is never entered.

### Key principle

The provider is an **image builder + sandbox factory** with thin caching hooks. It knows how to:
- `prepare()`: build an image from a Dockerfile and return an image ID
- `prewarm_image_cache()`: run the caching pipeline (delegates to `image_cache::run_prewarm_pipeline()`) and store the resulting image ID internally
- `prepare_from_checkpoint()`: build a thin-diff image on top of a base image from a patch file
- `create_sandbox()`: create a sandbox from the current image ID

All checkpoint logic, caching decisions, fallback strategies, and note management live in `image_cache.rs`. The provider stores the image_id internally after prewarm completes.

## Commit Sequence

Eight atomic commits. Each must pass `cargo fmt --check`, `cargo clippy`, `cargo nextest run`.

---

### Commit 1: Config schema -- `CheckpointConfig`

**Files:** `src/config/schema.rs`

Add `CheckpointConfig` struct:
```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CheckpointConfig {
    #[serde(default)]
    pub build_inputs: Vec<String>,
}
```

Add to `Config`:
```rust
#[serde(default)]
pub checkpoint: Option<CheckpointConfig>,
```

Validation (in `config.rs`): if `checkpoint` is `Some`, `build_inputs` must be non-empty; provider must not be `Local`.

Tests:
- `test_checkpoint_config_round_trip`
- `test_checkpoint_config_absent_defaults_to_none`

---

### Commit 2: Git module -- `src/git.rs`

**Files:** `src/git.rs` (new), `src/lib.rs`

New module encapsulating all git interactions. All functions use `tokio::task::spawn_blocking` with `std::process::Command` internally, so they are safe to call from async contexts. Each public function is `async` and returns `Result`.

Public API (all async functions take `repo: &Path` as the first argument, which is threaded through from the caller to allow running git commands in the correct directory):
```rust
pub const NOTES_REF: &str = "refs/notes/offload-images";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    pub image_id: String,
}

/// A note is a JSON object keyed by TOML config file path.
pub type NoteContents = HashMap<String, ImageEntry>;

pub async fn head_sha(repo: &Path) -> Result<String>;
pub async fn parent_sha(repo: &Path) -> Result<Option<String>>;  // None if initial commit
pub async fn repo_root(repo: &Path) -> Result<PathBuf>;
pub async fn read_note(repo: &Path, commit_sha: &str) -> Result<Option<NoteContents>>;
pub async fn write_note(repo: &Path, commit_sha: &str, contents: &NoteContents) -> Result<()>;
pub async fn push_notes(repo: &Path, remote: &str) -> Result<()>;
pub async fn fetch_notes(repo: &Path, remote: &str) -> Result<()>;
pub async fn configure_notes_fetch(repo: &Path, remote: &str) -> Result<()>;
pub async fn commit_touches_paths(repo: &Path, commit_sha: &str, paths: &[String]) -> Result<bool>;
pub async fn ancestors(repo: &Path, max_depth: usize) -> Result<Vec<String>>;
pub async fn export_tree(repo: &Path, commit_sha: &str, dest: &Path) -> Result<()>;
pub async fn generate_checkpoint_diff(repo: &Path, checkpoint_sha: &str) -> Result<Option<tempfile::NamedTempFile>>;
pub async fn diff_file_count(repo: &Path, from_sha: &str, to_sha: &str) -> Result<usize>;
pub fn canonicalize_config_path(config_path: &str, repo_root: &Path) -> Result<String>;
```

Implementation notes:
- `commit_touches_paths`: `git diff-tree --no-commit-id --name-only -r -m <sha>`, intersect with `paths`. The `-m` flag handles merge commits by checking against all parents.
- `ancestors`: `git log --format=%H -n <max_depth>`.
- `read_note` / `write_note`: read/write full JSON object, pretty-printed (indented) for human debuggability. `write_note` does a read-modify-write to merge entries (so two configs don't clobber each other). Config paths are canonicalized to repo-relative with no `./` prefix before use as keys. `git notes add -f` overwrites unconditionally.
- `push_notes`: force-push notes to remote unconditionally. Concurrency policy is last write wins -- notes are a write-through cache, so a clobbered entry simply triggers a rebuild on the next run that needs it. Returns `Ok(())` if remote ref doesn't exist yet (first push creates it).
- `configure_notes_fetch`: check `git config --get-all remote.<remote>.fetch` for existing refspec; add `+refs/notes/offload-images:refs/notes/offload-images` if absent.
- `fetch_notes`: returns `Ok(())` even if the remote ref doesn't exist (not an error on fresh repos).
- `read_note`: returns `Ok(None)` if the ref or note doesn't exist (not an error).
- `export_tree`: creates a shallow clone (depth=1) of the current repo at the given commit SHA via `git init` + `git fetch --depth=1` + `git checkout`, preserving `.git/` for downstream `COPY . /app` and `git apply`. Also creates a branch (`main`) so `refs/heads/` is non-empty.
- `generate_checkpoint_diff`: uses a temporary git index (`GIT_INDEX_FILE`) to produce a unified binary patch that includes both tracked modifications and untracked (non-ignored) files. The real index is never touched.
- `canonicalize_config_path`: strips `./` prefix, converts to repo-relative path. Used before any note read/write.

Tests (unit):
- `test_image_entry_json_round_trip`
- `test_canonicalize_config_path` (strips `./`, handles nested paths)
- `test_note_json_pretty_printed` (write note, read raw, verify indented)

Tests (integration, create temp git repos):
- `test_write_and_read_note`
- `test_write_note_merges_configs`
- `test_commit_touches_paths`
- `test_commit_touches_paths_merge_commit` (merge commit checks all parents)
- `test_configure_notes_fetch_idempotent`
- `test_read_note_missing_ref_returns_none`
- `test_fetch_notes_missing_ref_returns_ok`
- `test_export_tree`

---

### Commit 3: Checkpoint resolution logic -- `src/image_cache.rs`

**Files:** `src/image_cache.rs` (new), `src/lib.rs`

Image cache resolution and orchestration. Depends on `src/git.rs`. Contains the checkpoint resolution functions, the prewarm pipeline orchestration (`run_prewarm_pipeline()`, `try_thin_diff()`, `status_handler()`), base image building, note writing, and thin-diff generation.

```rust
pub struct CachedImage {
    pub image_id: String,
}

pub struct CheckpointInfo {
    pub checkpoint_sha: String,
    pub cached_image: Option<CachedImage>,
}

/// Find the nearest checkpoint ancestor and its cached image (if any).
/// Returns None if no checkpoint commit found within max_depth.
/// Takes repo: &Path as first argument (threaded through from caller).
pub async fn resolve_checkpoint(
    repo: &Path,
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<CheckpointInfo>>;

/// Find the nearest checkpoint ancestor SHA without reading git notes.
/// Used by --no-cache path which needs the SHA but not the cached image.
pub async fn find_checkpoint_sha(
    repo: &Path,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<String>>;
```

`resolve_latest_commit()` handles latest-commit resolution, and `resolve_base()` unifies both checkpoint and latest-commit resolution into a single entry point.

Logic for `resolve_checkpoint`:
1. `git::ancestors(max_depth)` to get commit SHAs.
2. For each SHA, `git::commit_touches_paths(sha, &cfg.build_inputs)` to find the nearest checkpoint.
3. If found: `git::read_note(sha)` and look up config key.
4. If note has image entry: return `CheckpointInfo { cached_image: Some(...) }`.
5. If note missing or no entry for this config: return `CheckpointInfo { cached_image: None }`.
6. If no checkpoint found in window: return `None`.

This module also contains `resolve_base()`, `run_prewarm_pipeline()`, `try_thin_diff()`, `write_note_for_commit()`, and `status_handler()`. The pipeline orchestration lives here, keeping `main.rs` focused on CLI dispatch.

Add `resolve_latest_commit()` for latest-commit caching (non-checkpoint mode). Returns the same shape as `resolve_checkpoint()` -- a base SHA and optional cached image -- so the caller can feed both into the same unified pipeline:
```rust
pub struct LatestCommitInfo {
    pub head_sha: String,
    pub cached_image: Option<CachedImage>,
}

/// In non-checkpoint mode: resolve the latest commit (HEAD) and its cached image (if any).
/// Returns None if empty repo (no commits).
/// Takes repo: &Path as first argument (threaded through from caller).
pub async fn resolve_latest_commit(
    repo: &Path,
    config_path: &str,
) -> Result<Option<LatestCommitInfo>>;
```

Logic: get HEAD SHA via `git::head_sha()`, read note, look up config key. Return `LatestCommitInfo { head_sha, cached_image: Some(...) }` on hit, `LatestCommitInfo { head_sha, cached_image: None }` on miss, or `None` if no commits.

Tests:
- `test_resolve_checkpoint_finds_nearest` (checkpoint 2 commits back)
- `test_resolve_checkpoint_none_when_no_match` (no build_inputs touched)
- `test_resolve_checkpoint_with_cached_image` (note exists with image entry)
- `test_resolve_latest_commit_hit` (HEAD has note)
- `test_resolve_latest_commit_miss` (HEAD has no note)
- `test_resolve_latest_commit_initial_commit` (no commits, returns None)

---

### Commit 4: `SandboxProvider` gains `prewarm_image_cache()` and `prepare_from_checkpoint()`

**Files:** `src/provider.rs`, `src/provider/modal.rs`, `src/provider/default.rs`, `src/provider/local.rs`

Two new methods are added to the `SandboxProvider` trait:

```rust
/// Prewarm the image cache by resolving a base commit and building a thin-diff image.
/// Providers that support image caching delegate to image_cache::run_prewarm_pipeline().
/// Providers that do not (Local) return CacheMiss.
async fn prewarm_image_cache(
    &mut self,
    ctx: &crate::image_cache::PrewarmContext<'_>,
) -> anyhow::Result<crate::image_cache::PrewarmOutcome>;

/// Build a thin-diff image from a checkpoint base image and a patch file.
/// Returns the new image ID if successful, or None for providers that
/// do not support this operation.
async fn prepare_from_checkpoint(
    &mut self,
    base_image_id: &str,
    patch_file: &Path,
    sandbox_project_root: &str,
    discovery_done: Option<&AtomicBool>,
) -> ProviderResult<Option<String>>;
```

`ModalProvider` and `DefaultProvider` implement `prewarm_image_cache()` by delegating to `image_cache::run_prewarm_pipeline(self, ctx)` and then storing the resulting image_id internally. They implement `prepare_from_checkpoint()` by building a `uv run @modal_sandbox.py prepare --from-base-image=... --patch-file=...` command. `LocalProvider` returns `CacheMiss` / `None` respectively.

**No `CheckpointContext`, no `CheckpointProvider` trait, no checkpoint branching in `build_prepare_command()`.** The provider's `prepare()` method is untouched -- it always does a normal Dockerfile-based build.

Tests:
- Existing provider tests pass unchanged

---

### Commit 5: Python `prepare` modifications

**Files:** `scripts/modal_sandbox.py`

**Remove old caching mechanism:**
- Delete `CACHE_FILE = ".offload-image-cache"`
- Delete `read_cached_image_id()`, `write_cached_image_id()`, `clear_image_cache()`
- The `--cached` flag is kept as a hidden, deprecated no-op for backward compatibility (not fully removed)
- Remove all call sites that read/write/clear the cache file

Image caching is now handled entirely by the Rust side via git notes. The Python script is a **thin wrapper around the Modal SDK** -- it builds images and returns IDs. It does not implement caching, fallback logic, or retry decisions. All such logic lives in Rust.

**Add checkpoint options to `prepare` command:**
- `--from-base-image` (image ID string)
- `--patch-file` (path to a binary patch file generated by Rust)
- `--sandbox-project-root` (default `/app`)

The Python script is a **thin wrapper** that applies a pre-generated patch. All git logic (diff generation, untracked file collection) lives in Rust. The Python script never runs git commands to generate diffs.

When `--from-base-image` is set:
1. `modal.Image.from_id(from_base_image)`
2. If `--patch-file` is not provided: return checkpoint image ID directly (print to stdout). This happens when the diff is empty (Rust determined no changes).
3. If `--patch-file` is provided:
   - `checkpoint_img.add_local_file(patch_file, "/tmp/offload.patch")`
   - `.run_commands(f"cd {project_root} && git apply /tmp/offload.patch --allow-empty && rm /tmp/offload.patch")`
   - Build, materialize, return new image ID
4. On image-expired or `git apply` failure: **exit non-zero**. Python does not implement fallback logic. All fallback/retry decisions live in Rust.

New helper:
- `_derive_image_from_base(app, base_img, patch_file, project_root) -> str`

Tests:
- `test_derive_image_no_patch` (no patch file, returns checkpoint image directly)
- `test_derive_image_with_patch` (patch file provided, applies and builds)

---

### Commit 6: Integrate into `offload run`

**Files:** `src/image_cache.rs`, `src/main.rs`

This is where the caching flow comes together. The integration follows the **linear fallthrough** pattern described in the Design section. The pipeline orchestration lives in `image_cache.rs`. The `main.rs` file calls `provider.prewarm_image_cache()` which delegates to `image_cache::run_prewarm_pipeline()`.

**ResolvedBase struct with BaseKind enum (in `image_cache.rs`):**

```rust
enum BaseKind {
    /// Nearest ancestor touching build_inputs (from [checkpoint] config).
    Checkpoint,
    /// Latest commit (HEAD).
    LatestCommit,
}

struct ResolvedBase {
    base_sha: String,
    cached_image_id: Option<String>,
    kind: BaseKind,
}
```

`BaseKind` carries only the diagnostic label (e.g. "[cache] Checkpoint hit" vs "[cache] Latest-commit hit").

**`PrewarmContext` and `PrewarmOutcome` structs (in `image_cache.rs`):**

```rust
pub struct PrewarmContext<'a> {
    pub repo: &'a Path,
    pub config: &'a Config,
    pub config_path: &'a Path,
    pub copy_dir_tuples: &'a [(PathBuf, PathBuf)],
    pub no_cache: bool,
    pub tracer: &'a Tracer,
    pub discovery_done: &'a AtomicBool,
}

pub enum PrewarmOutcome {
    Resolved { image_id: String },
    CacheMiss { base_sha: Option<String> },
}
```

The `CacheMiss` variant carries an `Option<String>` base_sha so the caller (`main.rs`) can write a cache note after a full build without re-resolving.

**Helper: `try_thin_diff()` (in `image_cache.rs`, replaces planned `build_thin_diff_image()`)**

An async function in `image_cache.rs` that generates the binary patch locally in Rust via `git::generate_checkpoint_diff()`, then routes through `provider.prepare_from_checkpoint()`. The provider builds the appropriate `--from-base-image` / `--patch-file` command internally.

Diff generation in Rust (via `git::generate_checkpoint_diff()`):
1. Create a temporary git index seeded with the checkpoint tree.
2. Stage the entire working tree (tracked + untracked) into the temp index via `git add -A`.
3. `git diff --cached --binary <checkpoint_sha>` against the temp index produces a unified binary patch.
4. If the diff is empty, return the base image ID directly (no Python call).
5. Otherwise, pass the patch file to `provider.prepare_from_checkpoint()`.

This keeps all git logic in Rust, consistent with the principle that Python is a thin SDK wrapper.

**Pipeline flow in `image_cache::run_prewarm_pipeline()`:**

```
resolve_base():
    if not in git repo: return None
    if !no_cache: fetch notes (best-effort), configure notes fetch
    if [checkpoint] config:
        if no_cache: find_checkpoint_sha() → ResolvedBase { kind: Checkpoint, cached_image_id: None }
        else: resolve_checkpoint() → ResolvedBase { kind: Checkpoint, base_sha, cached_image_id }
    else:
        if no_cache: head_sha() → ResolvedBase { kind: LatestCommit, cached_image_id: None }
        else: resolve_latest_commit() → ResolvedBase { kind: LatestCommit, base_sha, cached_image_id }
    (returns None if no base found)

Stage 1 (cache hit): if cached_image_id is Some(image_id):
    try_thin_diff(provider, base_image_id, base_sha, ...) → on success return Resolved
    on failure: warn, fall through to Stage 2

Stage 2 (cache miss / base build):
    export_tree(base_sha) to tempdir
    base_id = provider.prepare(context_dir=tempdir)
    if !no_cache: write note on base_sha, push
    try_thin_diff(provider, base_id, base_sha, ...) → on success return Resolved
    on failure: return CacheMiss { base_sha: Some(...) }
```

**In `main.rs`, the `run_remote_provider()` function:**

Discovery runs concurrently with the prewarm pipeline via `tokio::try_join!`:
```
(all_tests, prewarm_result) = tokio::try_join!(
    discover_with_signal(...),
    provider.prewarm_image_cache(&prewarm_ctx),
)
```

On `PrewarmOutcome::Resolved`: dispatch tests immediately (provider has image_id set).
On `PrewarmOutcome::CacheMiss { base_sha }`: snapshot working directory, run `provider.prepare()` as full build, write cache note if base_sha is present and caching enabled, then dispatch tests.

Note writing is best-effort (warn on failure, don't abort the run).

Tests:
- `test_run_with_checkpoint_cache_hit` (thin diff path)
- `test_run_with_checkpoint_cache_miss` (exports checkpoint tree, builds base from `context_dir`, then thin diff)
- `test_run_with_checkpoint_not_found` (falls through to full build)
- `test_run_with_no_cache_flag` (no `[checkpoint]` config: resolves HEAD SHA, exports HEAD tree, builds base from `context_dir`, thin diff of uncommitted changes -- no note read/write)
- `test_run_no_cache_with_checkpoint` (with `[checkpoint]` config: resolves checkpoint SHA, exports checkpoint tree, builds base via `context_dir`, applies thin diff -- no note read/write. Verifies `provider.prepare()` receives the exported tree as `context_dir`, not `None`.)
- `test_run_latest_commit_cache_hit` (thin diff from HEAD's cached image)
- `test_run_latest_commit_cache_miss` (exports HEAD tree, builds base from `context_dir`, caches on HEAD, then thin diff)
- `test_run_latest_commit_thin_diff_failure_falls_back`
- `test_run_latest_commit_no_commits` (empty repo, normal build)

---

### Commit 7: `offload checkpoint-status` command

**Files:** `src/main.rs` (CLI enum), `src/image_cache.rs` (handler logic)

Add to `Commands` enum in `main.rs`:
```rust
/// Show checkpoint cache status for the current HEAD.
CheckpointStatus {
    #[arg(long, default_value = "origin")]
    remote: String,
},
```

The handler function `status_handler()` lives in `image_cache.rs`. `main.rs` dispatches to `image_cache::status_handler()`.

`image_cache::status_handler()` flow:
1. Load config, fetch notes (best-effort), get HEAD SHA.
2. If `[checkpoint]` config is present:
   a. Use `image_cache::resolve_checkpoint()` to find the nearest ancestor touching `build_inputs`.
   b. If no checkpoint found in window: print "no checkpoint found in last N commits" and next run mode as full build.
   c. If found: compute distance from HEAD, read cached image entry, compute diff file count.
   d. Print summary with `(checkpoint, N commits back)` qualifier on base commit.
3. If `[checkpoint]` config is absent:
   a. Use `image_cache::resolve_latest_commit()` to resolve HEAD.
   b. If no commits (empty repo): print "no base commit" and next run mode as full build.
   c. If found: read cached image entry, compute diff file count.
   d. Print summary with `(latest commit, HEAD)` qualifier on base commit.
4. Print cached image ID (or "(none)") and next run mode (thin diff with file count, or full build reason).

Tests:
- `test_checkpoint_status_no_config` (no `[checkpoint]` section: shows latest-commit info via `resolve_latest_commit()`)
- `test_checkpoint_status_with_checkpoint` (shows checkpoint info with distance)
- `test_checkpoint_status_no_checkpoint_found` (reports no checkpoint in window)
- `test_checkpoint_status_no_commits` (empty repo: reports "no base commit", full build)

---

### Commit 8: Agent skills updates + cleanup

**Files:** `skills/offload/SKILL.md`, `skills/offload-onboard/SKILL.md`

- Add checkpoint section to offload skill
- Add optional checkpoint setup step to onboarding skill
- Update troubleshooting tables
- Remove old `#checkpoint-images.md#` draft

## Verification Checklist

Automated (every commit):
- [ ] `cargo fmt --check`
- [ ] `cargo clippy` (no warnings)
- [ ] `cargo nextest run`

Manual (after all commits, requires Modal credentials):

Checkpoint image caching (opt-in via `[checkpoint]` config):
- [ ] With `[checkpoint]`: only checkpoint commits trigger full rebuild
- [ ] Non-checkpoint commits after a checkpoint: thin diff applied
- [ ] Checkpoint cache miss: exports checkpoint tree, builds base from `context_dir`, writes note on checkpoint SHA, pushes, then thin diff
- [ ] Checkpoint cache hit: uses cached base image, builds thin diff
- [ ] Merge commit detected as checkpoint when it touches build_inputs via any parent

Latest-commit image caching (the default):
- [ ] Latest-commit caching first run (cache miss): exports HEAD tree, builds base from `context_dir`, note written on HEAD, then thin diff of uncommitted changes
- [ ] Latest-commit caching second run (cache hit): thin diff of uncommitted changes from HEAD's cached image
- [ ] Latest-commit caching empty repo: full build, no note (no commits)
- [ ] Latest-commit caching thin diff failure: falls back to full build

Both modes (unified pipeline -- identical steps after base commit resolution):
- [ ] Untracked files (non-gitignored) included in thin diff tarball
- [ ] `offload run --no-cache` without `[checkpoint]`: resolves HEAD SHA, exports HEAD tree, builds base from `context_dir`, thin diff of uncommitted changes -- no note interaction
- [ ] `offload run --no-cache` with `[checkpoint]`: resolves checkpoint SHA, exports checkpoint tree, builds base from `context_dir`, thin diff -- no note interaction. Verify `provider.prepare()` receives exported tree as `context_dir` (not `None`)
- [ ] Cached image expired: warns, rebuilds, updates note
- [ ] Two different TOML configs: separate entries in same note, no collision
- [ ] Note visible and pretty-printed: `git notes --ref=refs/notes/offload-images show HEAD` (latest-commit caching) or checkpoint SHA (checkpoint caching)
- [ ] Note pushed: `git ls-remote origin refs/notes/offload-images`
- [ ] `offload checkpoint-status`: shows base commit info and next run mode in both checkpoint and latest-commit modes
- [ ] Thin diff failure falls back to full build with warning

## Critical Files

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `CheckpointConfig`, wire into `Config` |
| `src/config.rs` | Validation for checkpoint config |
| `src/git.rs` | **New** -- all git notes and tree operations (`head_sha()`, `parent_sha()`, notes read/write, `generate_checkpoint_diff()`, `export_tree()`, etc.) |
| `src/image_cache.rs` | **New** -- checkpoint resolution (`resolve_checkpoint()`, `resolve_latest_commit()`), pipeline orchestration (`resolve_base()`, `run_prewarm_pipeline()`, `try_thin_diff()`), `status_handler()`, `write_note_for_commit()` |
| `src/lib.rs` | Register `git` and `image_cache` modules |
| `src/provider.rs` | Add `prewarm_image_cache()` and `prepare_from_checkpoint()` to `SandboxProvider` |
| `src/provider/modal.rs` | Implement `prewarm_image_cache()` (delegates to `image_cache::run_prewarm_pipeline()`) and `prepare_from_checkpoint()` |
| `src/provider/default.rs` | Implement `prewarm_image_cache()` (delegates to `image_cache::run_prewarm_pipeline()`) and `prepare_from_checkpoint()` |
| `src/provider/local.rs` | Implement `prewarm_image_cache()` (returns `CacheMiss`) and `prepare_from_checkpoint()` (returns `None`) |
| `src/main.rs` | `run_remote_provider()` calls `provider.prewarm_image_cache()` concurrently with discovery; handles `CacheMiss` fallback; `checkpoint-status` subcommand dispatches to `image_cache::status_handler()` |
| `scripts/modal_sandbox.py` | `--cached` kept as hidden deprecated no-op; add `--from-base-image` / `--patch-file` / `--sandbox-project-root`; add `_derive_image_from_base()` helper |
| `skills/offload/SKILL.md` | Checkpoint documentation |
| `skills/offload-onboard/SKILL.md` | Optional checkpoint step |
