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
| Separate `ImageBuilder` trait | `ImageBuilder` trait (`build_full`, `build_incremental`) lives in `image_cache.rs`. Providers implement both `ImageBuilder` and `SandboxProvider`. `SandboxProvider::prepare()` delegates to `prepare_with_prewarm(self, ctx)`, which is generic over `ImageBuilder`. | Splitting the image-build interface from the broader `SandboxProvider` keeps the prewarm pipeline testable via `MockImageBuilder` without standing up a real sandbox. The pipeline only needs `build_full` / `build_incremental`; isolating those into a narrow trait is cheaper to mock than the full provider surface. |
| Diff generation in Rust, application via `offload apply-diff` | Rust generates a unified binary patch using a temporary git index (`git read-tree` + `git add -A` + `git diff --cached --binary`), capturing both tracked changes and untracked files in one patch. The patch is shipped to the sandbox and applied by `offload apply-diff`, which uses the `diffy` crate. | Keeps diff generation in Rust; patch application uses offload's own binary (already installed in the sandbox image) instead of `git apply`. Eliminates `git apply --3way` failure modes while preserving the small-artifact architecture. |
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
    try_thin_diff(builder, image_id, base_sha) → return Resolved { image_id }
    on failure → fall through to Stage 2

Stage 2 (cache miss / base build):
  export_tree(base_sha)
  base_id = builder.build_full(context_dir=exported_tree, ...)
  if !no_cache: write_note(base_sha, base_id), push_notes
  try_thin_diff(builder, base_id, base_sha) → return Resolved { image_id }
  on failure → return CacheMiss { base_sha }

No base found → return CacheMiss { base_sha: None }
  // prepare_with_prewarm falls back to full_build_fallback, which snapshots
  // the working directory and calls builder.build_full().
```

`--no-cache` follows the same unified pipeline but skips all note interactions (no fetch, read, write, or push). It still resolves the base SHA, exports the tree, builds from `context_dir`, and applies thin diff -- producing the same image as a normal cache miss. The only difference is that the result is not persisted to git notes.

When `no_cache` is true, `resolve_base()` skips notes fetch and returns `ResolvedBase` with `cached_image_id: None`, ensuring the cache-hit stage is never entered.

### Key principle

Each provider implements two traits:

`ImageBuilder` (defined in `image_cache.rs`, `pub(crate)`):
- `build_full()`: build an image from scratch (Dockerfile + copy_dirs + sandbox_init_cmd).
- `build_incremental()`: build a thin-diff image on top of a base image given a patch file.

`SandboxProvider` (defined in `provider.rs`):
- `prepare()`: delegates to `prepare_with_prewarm(self, ctx)`. The prewarm pipeline runs the cache lookup + thin diff path; on miss it falls back to a full build via `full_build_fallback`. Stores the resulting image_id internally.
- `create_sandbox()`: create a sandbox from the current image ID.

All checkpoint logic, caching decisions, fallback strategies, and note management live in `image_cache.rs`. `prepare_with_prewarm` and `run_prewarm_pipeline` are generic over `ImageBuilder`, which enables unit-testing the pipeline against `MockImageBuilder` without standing up a real sandbox.

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
- `export_tree`: creates a shallow clone (depth=1) of the current repo at the given commit SHA via `git init` + `git fetch --depth=1` + `git checkout`. Also creates a branch (`main`) so `refs/heads/` is non-empty.
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

### Commit 4: `ImageBuilder` trait and `prepare_with_prewarm`

**Files:** `src/image_cache.rs`, `src/provider/modal.rs`, `src/provider/default.rs`, `src/provider/local.rs`

A new `pub(crate)` trait `ImageBuilder` is added to `image_cache.rs`:

```rust
#[async_trait]
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
```

`ModalProvider` and `DefaultProvider` implement `ImageBuilder`. Their `build_incremental` invokes the Python sandbox script:

```
uv run @modal_sandbox.py prepare --from-base-image=<id> --patch-file=<path> --sandbox-project-root=<root>
```

`LocalProvider` does **not** implement `ImageBuilder` (local doesn't build images). Its `SandboxProvider::prepare()` is a no-op for image work.

The existing `SandboxProvider::prepare()` is restructured to delegate:

```rust
// In ModalProvider / DefaultProvider:
async fn prepare(&mut self, ctx: &PrepareContext<'_>) -> ProviderResult<Option<String>> {
    let result = prepare_with_prewarm(self, ctx).await?;
    self.image_id = result.clone();
    Ok(result)
}
```

The `PrepareContext` struct already lives in `provider.rs` and is unchanged.

**No `CheckpointContext`, no `CheckpointProvider` trait, no checkpoint branching in `build_prepare_command()`.** The trait split keeps checkpoint/cache logic out of `SandboxProvider` entirely — it lives in `image_cache.rs` and is exercised through `ImageBuilder`.

Tests:
- `MockImageBuilder` in `image_cache.rs` exercises `run_prewarm_pipeline` without a real sandbox.
- Existing provider tests (sandbox-level) pass unchanged.

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

The Python script is a **thin wrapper** that ships the patch to the sandbox. All diff logic lives in Rust. The Python script never runs git commands to apply diffs — it delegates to `offload apply-diff`.

When `--from-base-image` is set:
1. `modal.Image.from_id(from_base_image)`
2. If `--patch-file` is not provided: return checkpoint image ID directly (print to stdout). This happens when the diff is empty (Rust determined no changes).
3. If `--patch-file` is provided:
   - `checkpoint_img.add_local_file(patch_file, "/tmp/offload.patch")`
   - `.run_commands(f"offload apply-diff /tmp/offload.patch --project-root {project_root} && rm /tmp/offload.patch")`
   - Build, materialize, return new image ID
4. On failure: **exit non-zero**. Python does not implement fallback logic. All fallback/retry decisions live in Rust.

New helper:
- `_derive_image_from_base(app, base_img, patch_file, project_root) -> str`

Tests:
- `test_derive_image_no_patch` (no patch file, returns checkpoint image directly)
- `test_derive_image_with_patch` (patch file provided, applies and builds)

---

### Commit 6: Integrate into `offload run`

**Files:** `src/image_cache.rs`, `src/main.rs`

This is where the caching flow comes together. The integration follows the **linear fallthrough** pattern described in the Design section. The pipeline orchestration lives in `image_cache.rs`. `main.rs` calls `provider.prepare(ctx)` (the existing `SandboxProvider::prepare` method); the provider's impl delegates to `image_cache::prepare_with_prewarm(self, ctx)`, which encapsulates cache lookup, thin diff, and full-build fallback.

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

**`PrewarmOutcome` enum (in `image_cache.rs`):**

```rust
pub(crate) enum PrewarmOutcome {
    Resolved { image_id: String },
    CacheMiss { base_sha: Option<String> },
}
```

The `CacheMiss` variant carries an `Option<String>` base_sha so `prepare_with_prewarm` can write a cache note after the fallback full build without re-resolving. `PrewarmOutcome` is internal to `image_cache.rs` -- callers in `main.rs` never see it; they only observe the final `Option<String>` image ID returned by `provider.prepare()`.

The `PrepareContext` struct already lives in `provider.rs` and is reused as-is by the prewarm pipeline.

**Helper: `try_thin_diff()` (in `image_cache.rs`)**

An async function generic over `ImageBuilder`. It generates the binary patch locally in Rust via `git::generate_checkpoint_diff()`, then calls `builder.build_incremental(base_image_id, patch_file, sandbox_project_root, discovery_done)`. The provider's `build_incremental` impl ships the patch to the sandbox, where `offload apply-diff` applies it using `diffy`.

Diff generation in Rust (via `git::generate_checkpoint_diff()`):
1. Create a temporary git index seeded with the checkpoint tree.
2. Stage the entire working tree (tracked + untracked) into the temp index via `git add -A`.
3. `git diff --cached --binary <checkpoint_sha>` against the temp index produces a unified binary patch.
4. If the diff is empty, return the base image ID directly (no Python call).
5. Otherwise, pass the patch file to `builder.build_incremental(...)`.

This keeps diff generation in Rust, consistent with the principle that Python is a thin SDK wrapper. Patch application happens in the sandbox via `offload apply-diff`.

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
    try_thin_diff(builder, image_id, base_sha, ...) → on success return Resolved
    on failure: warn, fall through to Stage 2

Stage 2 (cache miss / base build):
    export_tree(base_sha) to tempdir
    base_id = builder.build_full(context_dir=tempdir, ...)
    if !no_cache: write note on base_sha, push
    try_thin_diff(builder, base_id, base_sha, ...) → on success return Resolved
    on failure: return CacheMiss { base_sha: Some(...) }
```

`prepare_with_prewarm` wraps `run_prewarm_pipeline`: on `Resolved` it returns the image ID; on `CacheMiss` it invokes `full_build_fallback` (which snapshots the working directory, runs `build_full`, and writes a cache note when `base_sha` is present and caching is enabled).

**In `main.rs`, the prepare invocation:**

Discovery runs concurrently with `run_prepare` (which builds a `PrepareContext` and calls `provider.prepare(ctx)`):
```
(all_tests, _) = tokio::try_join!(
    discover_with_signal(...),
    run_prepare(&mut provider, repo, config, config_path, copy_dir_tuples, no_cache, tracer, &discovery_done),
)
```

`main.rs` does not branch on cache hit/miss -- the entire pipeline is encapsulated inside `provider.prepare()`. Note writing is best-effort within the pipeline (warn on failure, don't abort the run).

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
- [ ] Untracked files (non-gitignored) included in thin diff patch
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
| `src/image_cache.rs` | **New** -- `ImageBuilder` trait (`build_full`, `build_incremental`); checkpoint resolution (`resolve_checkpoint()`, `resolve_latest_commit()`); pipeline orchestration (`resolve_base()`, `run_prewarm_pipeline()`, `prepare_with_prewarm()`, `full_build_fallback()`, `try_thin_diff()`); `status_handler()`; `write_note_for_commit()` |
| `src/lib.rs` | Register `git` and `image_cache` modules |
| `src/provider.rs` | `PrepareContext<'a>` already exists -- no trait changes |
| `src/provider/modal.rs` | Implement `ImageBuilder` (`build_full`, `build_incremental`); `SandboxProvider::prepare()` delegates to `prepare_with_prewarm(self, ctx)` |
| `src/provider/default.rs` | Implement `ImageBuilder` (`build_full`, `build_incremental`); `SandboxProvider::prepare()` delegates to `prepare_with_prewarm(self, ctx)` |
| `src/provider/local.rs` | Does not implement `ImageBuilder` (local doesn't build images); `SandboxProvider::prepare()` is a no-op |
| `src/main.rs` | `run_prepare()` builds `PrepareContext` and calls `provider.prepare(ctx)` concurrently with discovery via `tokio::try_join!`; cache and fallback logic is encapsulated inside `prepare_with_prewarm` (no main.rs branching). `checkpoint-status` subcommand dispatches to `image_cache::status_handler()` |
| `scripts/modal_sandbox.py` | `--cached` kept as hidden deprecated no-op; add `--from-base-image` / `--patch-file` / `--sandbox-project-root`; add `_derive_image_from_base()` helper |
| `skills/offload/SKILL.md` | Checkpoint documentation |
| `skills/offload-onboard/SKILL.md` | Optional checkpoint step |
