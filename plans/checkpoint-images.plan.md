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
| Provider `prepare()` unchanged | Checkpoint thin-diff is a separate call, not routed through `prepare()` | `prepare()` builds images from Dockerfiles — that's its job. The thin-diff step is orchestrated by `main.rs` using `run_prepare_command()` directly. Keeps providers simple and single-purpose |
| Diff generation in Rust | Rust generates the binary patch file (git diff + untracked files) and passes `--patch-file` to Python | Keeps all git logic in Rust; Python is a thin SDK wrapper that only applies a pre-generated patch via Modal API calls |
| Fallthrough caching | Cache lookups are transparent steps in a linear pipeline | No scattered if/else trees; each step either produces a value or falls through to the next |
| Unified caching pipeline | Checkpoint and ParentBase follow identical steps after base-commit resolution | The only difference is how the base commit is selected (nearest ancestor touching `build_inputs` vs HEAD~1). Everything after resolution -- cache lookup, tree export, base build, thin diff, note write -- is the same code path. Variant names are kept for logging/diagnostics |
| Parent-commit base commit | Use HEAD~1 (parent commit) as base image | HEAD~1 is stable (won't change); caching on HEAD is wrong because prepare() reflects the working tree; thin diff from parent is typically one commit of changes |
| `--no-cache` preserves build procedure | `--no-cache` skips note interactions but still exports the base commit tree and builds from `context_dir` | Both Checkpoint and ParentBase paths export a clean tree and pass it as `context_dir` so `COPY . /app` gets a deterministic checkout. Falling through to a plain full build uses `context_dir=None`, which copies the live CWD (includes `.git/`, untracked files, etc.) -- producing a different and likely broken image. `--no-cache` means "don't use the cache," not "use a different build procedure." |

## Design: Caching Flow

The caching flow is a **linear fallthrough pipeline**. Each step either succeeds (short-circuits) or falls through to the next.

### Base commit resolution

The first step determines the base commit. This is the **only** point where Checkpoint and ParentBase diverge:

```
resolve_base_commit():
  if [checkpoint] config present:
    walk ancestors, find first commit touching build_inputs
    read note on that commit → ResolvedBase::Checkpoint { base_sha, cached_image_id }
    (returns None if no checkpoint found in window)
  else:
    use HEAD~1 as base commit
    read note on parent → ResolvedBase::ParentBase { base_sha, cached_image_id }
    (returns None if initial commit with no parent)
```

The variant name (Checkpoint vs ParentBase) is used only for log messages (e.g. "[cache] Checkpoint hit" vs "[cache] Parent-commit hit").

### Unified pipeline (after resolution)

Both variants follow identical steps:

```
match resolved_base:
  Some(base) with cached_image_id = Some(image_id):
    // Cache hit (Checkpoint or ParentBase -- same path)
    thin_diff(image_id, base_sha) → set_image_id
    on failure → fall through to full build

  Some(base) with cached_image_id = None:
    // Cache miss (Checkpoint or ParentBase -- same path)
    export_tree(base_sha)
    prepare(context_dir=exported_tree)
    write_note(base_sha, image_id)
    thin_diff(image_id, base_sha) → set_image_id
    on failure → fall through to full build

  None:
    // No base found (no checkpoint in window, or initial commit with no parent)
    full build, no caching
```

`--no-cache` follows the same unified pipeline but skips all note interactions (no fetch, read, write, or push). It still resolves the base SHA, exports the tree, builds from `context_dir`, and applies thin diff -- producing the same image as a normal cache miss. The only difference is that the result is not persisted to git notes.

### Key principle

The provider is a **dumb image builder + sandbox factory**. It knows how to:
- `prepare()`: build an image from a Dockerfile and return an image ID
- `set_image_id()`: accept an externally-provided image ID
- `create_sandbox()`: create a sandbox from the current image ID

All checkpoint logic, caching decisions, fallback strategies, and note management live in `main.rs` and `checkpoint.rs`. The provider never sees `CheckpointContext`.

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

Public API:
```rust
pub const NOTES_REF: &str = "refs/notes/offload-images";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    pub image_id: String,
}

/// A note is a JSON object keyed by TOML config file path.
pub type NoteContents = HashMap<String, ImageEntry>;

pub async fn head_sha() -> Result<String>;
pub async fn parent_sha() -> Result<Option<String>>;  // None if initial commit
pub async fn repo_root() -> Result<PathBuf>;
pub async fn read_note(commit_sha: &str) -> Result<Option<NoteContents>>;
pub async fn write_note(commit_sha: &str, contents: &NoteContents) -> Result<()>;
pub async fn push_notes(remote: &str) -> Result<()>;
pub async fn fetch_notes(remote: &str) -> Result<()>;
pub async fn configure_notes_fetch(remote: &str) -> Result<()>;
pub async fn commit_touches_paths(commit_sha: &str, paths: &[String]) -> Result<bool>;
pub async fn ancestors(max_depth: usize) -> Result<Vec<String>>;
pub async fn export_tree(commit_sha: &str, dest: &Path) -> Result<()>;
pub async fn diff_file_count(from_sha: &str, to_sha: &str) -> Result<usize>;
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
- `export_tree`: shallow clone of repo at a specific SHA into a temp dir, preserving `.git/` for downstream `COPY . /app` and `git apply`.
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

### Commit 3: Checkpoint resolution logic -- `src/checkpoint.rs`

**Files:** `src/checkpoint.rs` (new), `src/lib.rs`

Read-only checkpoint resolution. Depends on `src/git.rs`. Does NOT build images or call providers -- it only reads existing cached data. Building, caching (writing notes, pushing), and provider orchestration are done by the caller in `main.rs`.

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
pub async fn resolve_checkpoint(
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    max_depth: usize,
) -> Result<Option<CheckpointInfo>>;

/// Check if HEAD has a cached image in git notes.
/// Returns the image_id if found, None otherwise.
pub async fn resolve_cached_image(
    config_path: &str,
) -> Result<Option<String>>;
```

Logic for `resolve_checkpoint`:
1. `git::ancestors(max_depth)` to get commit SHAs.
2. For each SHA, `git::commit_touches_paths(sha, &cfg.build_inputs)` to find the nearest checkpoint.
3. If found: `git::read_note(sha)` and look up config key.
4. If note has image entry: return `CheckpointInfo { cached_image: Some(...) }`.
5. If note missing or no entry for this config: return `CheckpointInfo { cached_image: None }`.
6. If no checkpoint found in window: return `None`.

Note: this module is intentionally thin. It only answers "where is the checkpoint?" and "is there a cached image?". All decisions about what to do with that information live in `main.rs`.

Add `resolve_parent_base()` for parent-commit caching (non-checkpoint mode). Returns the same shape as `resolve_checkpoint()` -- a base SHA and optional cached image -- so the caller can feed both into the same unified pipeline:
```rust
pub struct ParentBaseInfo {
    pub parent_sha: String,
    pub cached_image: Option<CachedImage>,
}

/// In non-checkpoint mode: resolve the parent commit and its cached image (if any).
/// Returns None if initial commit (no parent).
pub async fn resolve_parent_base(
    config_path: &str,
) -> Result<Option<ParentBaseInfo>>;
```

Logic: get HEAD's parent (HEAD~1 via `git::parent_sha()`), read note, look up config key. Return `ParentBaseInfo { parent_sha, cached_image: Some(...) }` on hit, `ParentBaseInfo { parent_sha, cached_image: None }` on miss, or `None` if initial commit.

Tests:
- `test_resolve_checkpoint_finds_nearest` (checkpoint 2 commits back)
- `test_resolve_checkpoint_none_when_no_match` (no build_inputs touched)
- `test_resolve_checkpoint_with_cached_image` (note exists with image entry)
- `test_resolve_parent_base_hit` (parent has note)
- `test_resolve_parent_base_miss` (parent has no note)
- `test_resolve_parent_base_initial_commit` (no parent, returns None)

---

### Commit 4: `SandboxProvider` gains `set_image_id()`

**Files:** `src/provider.rs`, `src/provider/modal.rs`, `src/provider/default.rs`

Minimal provider change: add `set_image_id()` to `SandboxProvider` so that callers can inject an externally-obtained image ID (from cache or from a separate build step) without going through `prepare()`.

Add to `SandboxProvider` trait:
```rust
/// Set the image ID directly, bypassing prepare().
/// Used when the image was obtained from cache or built externally.
/// Default implementation is a no-op (for providers like Local that don't use image IDs).
fn set_image_id(&mut self, _id: String) {}
```

`ModalProvider` and `DefaultProvider` override to set their internal `image_id` field:
```rust
fn set_image_id(&mut self, id: String) {
    self.image_id = Some(id);
}
```

**No other provider changes.** No `CheckpointContext`, no `CheckpointProvider` trait, no checkpoint branching in `build_prepare_command()`. The provider's `prepare()` method is untouched -- it always does a normal Dockerfile-based build.

Tests:
- Existing provider tests pass unchanged

---

### Commit 5: Python `prepare` modifications

**Files:** `scripts/modal_sandbox.py`

**Remove old caching mechanism:**
- Delete `CACHE_FILE = ".offload-image-cache"`
- Delete `read_cached_image_id()`, `write_cached_image_id()`, `clear_image_cache()`
- Remove `--cached` flag from `prepare` command
- Remove all call sites that read/write/clear the cache file

Image caching is now handled entirely by the Rust side via git notes. The Python script is a **thin wrapper around the Modal SDK** -- it builds images and returns IDs. It does not implement caching, fallback logic, or retry decisions. All such logic lives in Rust.

**Add checkpoint options to `prepare` command:**
- `--from-checkpoint` (image ID string)
- `--patch-file` (path to a binary patch file generated by Rust)
- `--sandbox-project-root` (default `/app`)

The Python script is a **thin wrapper** that applies a pre-generated patch. All git logic (diff generation, untracked file collection) lives in Rust. The Python script never runs git commands to generate diffs.

When `--from-checkpoint` is set:
1. `modal.Image.from_id(from_checkpoint)`
2. If `--patch-file` is not provided: return checkpoint image ID directly (print to stdout). This happens when the diff is empty (Rust determined no changes).
3. If `--patch-file` is provided:
   - `checkpoint_img.add_local_file(patch_file, "/tmp/offload.patch")`
   - `.run_commands(f"cd {project_root} && git apply /tmp/offload.patch --allow-empty && rm /tmp/offload.patch")`
   - Build, materialize, return new image ID
4. On image-expired or `git apply` failure: **exit non-zero**. Python does not implement fallback logic. All fallback/retry decisions live in Rust.

New helper:
- `_derive_image_from_checkpoint(app, checkpoint_img, patch_file, project_root) -> str`

Tests:
- `test_derive_image_no_patch` (no patch file, returns checkpoint image directly)
- `test_derive_image_with_patch` (patch file provided, applies and builds)

---

### Commit 6: Integrate into `offload run`

**Files:** `src/main.rs`

This is where the caching flow comes together. The integration follows the **linear fallthrough** pattern described in the Design section. All checkpoint logic, caching, note management, and fallback decisions live here -- providers know nothing about checkpoints.

**ResolvedBase enum:**

```rust
enum ResolvedBase {
    /// Base commit from [checkpoint] config: nearest ancestor touching build_inputs.
    Checkpoint {
        base_sha: String,
        cached_image_id: Option<String>,
    },
    /// Base commit from parent: HEAD~1.
    ParentBase {
        base_sha: String,
        cached_image_id: Option<String>,
    },
}
```

Both variants have identical structure. The variant name is kept for logging/diagnostics (e.g. "[cache] Checkpoint hit" vs "[cache] Parent-commit hit").

**Helper: `build_thin_diff_image()`**

A standalone async function in `main.rs` that generates the binary patch locally in Rust, then calls the Python script with `--from-checkpoint` and `--patch-file` flags. Uses `run_prepare_command()` (already `pub(crate)` in `src/provider.rs`) with a fresh `ShellConnector`. Does NOT go through the provider's `prepare()`.

Diff generation in Rust (before calling Python):
1. Create a temporary directory for the patch file.
2. Generate the binary diff: `git diff <base_sha> HEAD --binary` and write to a temp file.
3. Collect untracked files: `git ls-files --others --exclude-standard`.
4. If untracked files exist, append them to the patch file (or bundle as a combined archive).
5. If the diff is empty and no untracked files exist, skip calling Python and return the base image ID directly.
6. Otherwise, call the Python script with `--from-checkpoint=<image_id> --patch-file=<path>`.

This keeps all git logic in Rust, consistent with the principle that Python is a thin SDK wrapper.

```rust
/// Build a thin-diff image on top of a base image.
/// Generates the binary patch locally (git diff + untracked files),
/// then calls the Python script with --from-checkpoint and --patch-file.
/// Returns the target image ID on success.
async fn build_thin_diff_image(
    base_image_id: &str,
    base_sha: &str,
    sandbox_project_root: &str,
    discovery_done: Option<&AtomicBool>,
) -> ProviderResult<String>;
```

**In `run_tests()`, after loading config and resolving copy_dirs/env:**

```
if local provider:
    skip all caching, proceed to normal prepare+dispatch

if --no-cache:
    // Skip all note interactions, but preserve build procedure.
    // Both Checkpoint and ParentBase follow the same path here.
    resolve base SHA (same logic as normal, but skip note reading):
        if [checkpoint] config:
            walk ancestors looking for checkpoint commit
            base_sha = checkpoint SHA (or None if not found)
        else:
            base_sha = HEAD~1 (or None if initial commit)
    if base_sha found:
        export_tree(base_sha) to tempdir
        base_id = provider.prepare(context_dir=tempdir)
        // Do NOT write note, do NOT push
        thin_diff_result = build_thin_diff_image(base_id, base_sha, ...)
        if thin_diff_result is Ok:
            provider.set_image_id(target_id)
        else:
            warn, fall through to full build
    else:
        // No base found: full build
        provider.prepare() (normal full build)
    proceed to dispatch (skip resolve_base and match below)

resolve_base():
    fetch notes (best-effort)
    if [checkpoint] config:
        resolve_checkpoint() → ResolvedBase::Checkpoint { base_sha, cached_image_id }
    else:
        resolve_parent_base() → ResolvedBase::ParentBase { base_sha, cached_image_id }
    (returns None if no base found)

match resolved_base:
    // Cache hit: base image exists, build thin diff
    // (same for both Checkpoint and ParentBase)
    Some(base) where cached_image_id is Some(image_id) →
        thin_diff_result = build_thin_diff_image(image_id, base_sha, ...)
        if thin_diff_result is Ok:
            provider.set_image_id(target_id)
        else:
            warn, fall through to full build

    // Cache miss: export tree, build base image, cache it, then thin diff
    // (same for both Checkpoint and ParentBase)
    Some(base) where cached_image_id is None →
        export_tree(base_sha) to tempdir
        base_id = provider.prepare(context_dir=tempdir)
        write note on base_sha, push
        thin_diff_result = build_thin_diff_image(base_id, base_sha, ...)
        if thin_diff_result is Ok:
            provider.set_image_id(target_id)
        else:
            warn, fall through to full build

    // No base found: normal build
    None →
        provider.prepare() (normal full build, no caching)
```

The `discover_with_signal()` + `prepare()` concurrency pattern is preserved: discovery and prepare run concurrently via `tokio::try_join!`.

Note writing is best-effort (warn on failure, don't abort the run).

Tests:
- `test_run_with_checkpoint_cache_hit` (thin diff path)
- `test_run_with_checkpoint_cache_miss` (exports checkpoint tree, builds base from `context_dir`, then thin diff)
- `test_run_with_checkpoint_not_found` (falls through to full build)
- `test_run_with_no_cache_flag` (no `[checkpoint]` config: resolves parent SHA, exports parent tree, builds base from `context_dir`, thin diff -- no note read/write)
- `test_run_no_cache_with_checkpoint` (with `[checkpoint]` config: resolves checkpoint SHA, exports checkpoint tree, builds base via `context_dir`, applies thin diff -- no note read/write. Verifies `provider.prepare()` receives the exported tree as `context_dir`, not `None`.)
- `test_run_parent_commit_cache_hit` (thin diff from parent's cached image)
- `test_run_parent_commit_cache_miss` (exports parent tree, builds base from `context_dir`, caches on parent, then thin diff)
- `test_run_parent_commit_thin_diff_failure_falls_back`
- `test_run_parent_commit_initial_commit` (no parent, normal build)

---

### Commit 7: `offload checkpoint-status` command

**Files:** `src/main.rs`

Add to `Commands` enum:
```rust
/// Show checkpoint cache status for the current HEAD.
CheckpointStatus {
    #[arg(long, default_value = "origin")]
    remote: String,
},
```

`checkpoint_status_handler()` flow:
1. Load config, fetch notes (best-effort), get HEAD SHA.
2. If `[checkpoint]` config is present:
   a. Use `checkpoint::resolve_checkpoint()` to find the nearest ancestor touching `build_inputs`.
   b. If no checkpoint found in window: print "no checkpoint found in last N commits" and next run mode as full build.
   c. If found: compute distance from HEAD, read cached image entry, compute diff file count.
   d. Print summary with `(checkpoint, N commits back)` qualifier on base commit.
3. If `[checkpoint]` config is absent:
   a. Use `checkpoint::resolve_parent_base()` to resolve HEAD~1.
   b. If no parent (initial commit): print "no base commit" and next run mode as full build.
   c. If found: read cached image entry, compute diff file count.
   d. Print summary with `(parent, HEAD~1)` qualifier on base commit.
4. Print cached image ID (or "(none)") and next run mode (thin diff with file count, or full build reason).

Tests:
- `test_checkpoint_status_no_config` (no `[checkpoint]` section: shows parent-commit info via `resolve_parent_base()`)
- `test_checkpoint_status_with_checkpoint` (shows checkpoint info with distance)
- `test_checkpoint_status_no_checkpoint_found` (reports no checkpoint in window)
- `test_checkpoint_status_initial_commit` (no parent: reports "no base commit", full build)

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

Parent-commit image caching (the default):
- [ ] Parent-commit caching first run (cache miss): exports parent tree, builds base from `context_dir`, note written on parent commit (HEAD~1), then thin diff
- [ ] Parent-commit caching second run (cache hit): thin diff from parent's cached image
- [ ] Parent-commit caching initial commit: full build, no note (no parent)
- [ ] Parent-commit caching thin diff failure: falls back to full build

Both modes (unified pipeline -- identical steps after base commit resolution):
- [ ] Untracked files (non-gitignored) included in thin diff tarball
- [ ] `offload run --no-cache` without `[checkpoint]`: resolves parent SHA, exports parent tree, builds base from `context_dir`, thin diff -- no note interaction
- [ ] `offload run --no-cache` with `[checkpoint]`: resolves checkpoint SHA, exports checkpoint tree, builds base from `context_dir`, thin diff -- no note interaction. Verify `provider.prepare()` receives exported tree as `context_dir` (not `None`)
- [ ] Cached image expired: warns, rebuilds, updates note
- [ ] Two different TOML configs: separate entries in same note, no collision
- [ ] Note visible and pretty-printed: `git notes --ref=refs/notes/offload-images show HEAD~1` (parent-commit caching) or checkpoint SHA (checkpoint caching)
- [ ] Note pushed: `git ls-remote origin refs/notes/offload-images`
- [ ] `offload checkpoint-status`: shows base commit info and next run mode in both checkpoint and parent-commit modes
- [ ] Thin diff failure falls back to full build with warning

## Critical Files

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `CheckpointConfig`, wire into `Config` |
| `src/config.rs` | Validation for checkpoint config |
| `src/git.rs` | **New** -- all git notes and tree operations (`head_sha()`, `parent_sha()`, notes read/write, etc.) |
| `src/checkpoint.rs` | **New** -- read-only checkpoint resolution (`resolve_checkpoint()`, `resolve_parent_base()`; no provider dependency) |
| `src/lib.rs` | Register `git` and `checkpoint` modules |
| `src/provider.rs` | Add `set_image_id()` to `SandboxProvider` (default no-op) |
| `src/provider/modal.rs` | Override `set_image_id()` |
| `src/provider/default.rs` | Override `set_image_id()` |
| `src/main.rs` | Unified caching pipeline (`ResolvedBase` enum), `build_thin_diff_image()`, `checkpoint-status` subcommand |
| `scripts/modal_sandbox.py` | Remove `--cached` + cache file functions; add `--from-checkpoint` / `--patch-file` / `--sandbox-project-root` |
| `skills/offload/SKILL.md` | Checkpoint documentation |
| `skills/offload-onboard/SKILL.md` | Optional checkpoint step |
