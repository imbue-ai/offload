# Checkpoint Images -- Implementation Plan

Implementation plan for the checkpoint images spec (`checkpoint-images.spec.md`).

## Key Technical Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Git operations | Shell out to `git` CLI via `spawn_blocking` | No `git2` crate (heavy native dep); works with jj colocated repos; `spawn_blocking` prevents stalling the tokio runtime |
| Build inputs hashing | `git hash-object --stdin` (SHA-1) | Already available; avoids new `sha2` crate; sufficient for change detection |
| Notes content | JSON keyed by TOML config path | Prevents collision when multiple configs target different Dockerfiles |
| `.offload-image-cache` | Remove from `modal_sandbox.py` | Git notes are the sole caching mechanism; `.offload-image-cache` is superseded |
| Checkpoint detection | `git diff-tree --no-commit-id --name-only -r -m <sha>` | Handles merge commits (all parents); pure function of commit content |
| Config path keys | Repo-relative, no `./` prefix | Canonical form prevents duplicate entries |
| JSON in notes | Pretty-printed (indented) | Human-debuggable via `git notes show` |

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_inputs_hash: Option<String>,
}

/// A note is a JSON object keyed by TOML config file path.
pub type NoteContents = HashMap<String, ImageEntry>;

pub async fn head_sha() -> Result<String>;
pub async fn repo_root() -> Result<PathBuf>;
pub async fn compute_build_inputs_hash(repo_root: &Path, files: &[String]) -> Result<String>;
pub async fn read_note(commit_sha: &str) -> Result<Option<NoteContents>>;
pub async fn write_note(commit_sha: &str, contents: &NoteContents) -> Result<()>;
pub async fn push_notes(remote: &str) -> Result<()>;
pub async fn fetch_notes(remote: &str) -> Result<()>;
pub async fn configure_notes_fetch(remote: &str) -> Result<()>;
pub async fn commit_touches_paths(commit_sha: &str, paths: &[String]) -> Result<bool>;
pub async fn ancestors(max_depth: usize) -> Result<Vec<String>>;
pub fn canonicalize_config_path(config_path: &str, repo_root: &Path) -> Result<String>;
```

Implementation notes:
- `compute_build_inputs_hash`: sort files lexicographically; for each, prepend `FILE:<path>:<len>\n` header; concatenate all; pipe to `git hash-object --stdin`. **Error if any file in `build_inputs` does not exist** (prevents silent hash changes from deleted files).
- `commit_touches_paths`: `git diff-tree --no-commit-id --name-only -r -m <sha>`, intersect with `paths`. The `-m` flag handles merge commits by checking against all parents.
- `ancestors`: `git log --format=%H -n <max_depth>`.
- `read_note` / `write_note`: read/write full JSON object, pretty-printed (indented) for human debuggability. `write_note` does a read-modify-write to merge entries (so two configs don't clobber each other). Config paths are canonicalized to repo-relative with no `./` prefix before use as keys. `git notes add -f` overwrites unconditionally.
- `push_notes`: force-push notes to remote unconditionally. Concurrency policy is last write wins -- notes are a write-through cache, so a clobbered entry simply triggers a rebuild on the next run that needs it. Returns `Ok(())` if remote ref doesn't exist yet (first push creates it).
- `configure_notes_fetch`: check `git config --get-all remote.<remote>.fetch` for existing refspec; add `+refs/notes/offload-images:refs/notes/offload-images` if absent.
- `fetch_notes`: returns `Ok(())` even if the remote ref doesn't exist (not an error on fresh repos).
- `read_note`: returns `Ok(None)` if the ref or note doesn't exist (not an error).
- `canonicalize_config_path`: strips `./` prefix, converts to repo-relative path. Used before any note read/write.

Tests (unit):
- `test_image_entry_json_round_trip`
- `test_build_inputs_hash_deterministic` (write temp files, call twice, assert equal)
- `test_build_inputs_hash_content_sensitive` (change a file, assert different hash)
- `test_build_inputs_hash_missing_file_errors` (missing file returns error, not silent)
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

---

### Commit 3: Checkpoint resolution logic -- `src/checkpoint.rs`

**Files:** `src/checkpoint.rs` (new), `src/lib.rs`

High-level checkpoint resolution, called from `offload run`. Depends on `src/git.rs`.

```rust
pub struct ResolvedCheckpoint {
    pub checkpoint_sha: String,
    pub image_id: String,
}

/// Find the nearest checkpoint ancestor, returning its SHA and cached image
/// (building and caching if needed).
pub async fn resolve_checkpoint(
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    provider: &mut impl SandboxProvider,
    max_depth: usize,
) -> Result<Option<ResolvedCheckpoint>>;

/// For non-checkpoint mode: check if HEAD has a cached image, build if not.
pub async fn resolve_cached_image(
    config_path: &str,
    provider: &mut impl SandboxProvider,
) -> Result<Option<String>>;  // image_id
```

Logic for `resolve_checkpoint`:
1. `git::ancestors(max_depth)` to get commit SHAs.
2. For each SHA, `git::commit_touches_paths(sha, &cfg.build_inputs)` to find the nearest checkpoint.
3. If found: `git::read_note(sha)` and look up config key.
4. If note has image: verify build inputs hash, return.
5. If note missing or no entry for this config: build image, write note (read-modify-write), push, return.
6. If no checkpoint found: return `None`.

---

### Commit 4: Provider checkpoint support

**Files:** `src/provider/modal.rs`, `src/provider/default.rs`

Add `CheckpointContext` struct:
```rust
pub struct CheckpointContext {
    pub image_id: String,
    pub commit_sha: String,
    pub sandbox_project_root: String,
}
```

Add to `ModalProvider`:
- Field: `checkpoint: Option<CheckpointContext>`
- Builder: `with_checkpoint(mut self, ctx: CheckpointContext) -> Self`

Modify `build_prepare_command()`:
- When `self.checkpoint` is `Some`: append `--from-checkpoint=<image_id> --checkpoint-sha=<sha> --sandbox-project-root=<root>`. Omit `--include-cwd`, `--copy-dir`, `--sandbox-init-cmd`.
- When `None`: existing behavior unchanged.

Apply same pattern to `DefaultProvider`.

Tests:
- `test_prepare_command_with_checkpoint`
- `test_prepare_command_without_checkpoint` (existing tests still pass)

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
- `--checkpoint-sha` (git SHA string)
- `--sandbox-project-root` (default `/app`)

When `--from-checkpoint` is set:
1. `modal.Image.from_id(from_checkpoint)`
2. Generate diff locally: `git diff <checkpoint_sha> HEAD --binary`
3. Collect untracked files: `git ls-files --others --exclude-standard`
4. If diff is empty and no untracked files: return checkpoint image ID (print to stdout)
5. If non-empty:
   - Write diff to temp file
   - Tar untracked files (if any) into a separate archive
   - `checkpoint_img.add_local_file(diff_path, "/tmp/offload.patch")`
   - If untracked archive exists: `checkpoint_img.add_local_file(untracked_tar, "/tmp/offload-untracked.tar")`
   - `.run_commands(f"cd {project_root} && git apply /tmp/offload.patch --allow-empty && rm /tmp/offload.patch")`
   - If untracked archive: `.run_commands(f"cd {project_root} && tar xf /tmp/offload-untracked.tar && rm /tmp/offload-untracked.tar")`
   - Build, materialize, return new image ID
6. On image-expired or `git apply` failure: **exit non-zero**. Python does not implement fallback logic. All fallback/retry decisions live in Rust (see design principle in `ARCHITECTURE.md`).

New helpers:
- `_generate_git_diff(checkpoint_sha: str) -> str | None`
- `_collect_untracked_files() -> list[str]`
- `_build_run_image_from_checkpoint(app, checkpoint_img, diff: str | None, untracked_tar: str | None, project_root: str) -> str`

Tests:
- `test_generate_git_diff_empty` (no changes since checkpoint)
- `test_generate_git_diff_nonempty` (changes present)
- `test_collect_untracked_files` (detects new files, ignores gitignored)

---

### Commit 6: Integrate into `offload run`

**Files:** `src/main.rs`

In `run_tests()`, after loading config and before provider dispatch:

**With `[checkpoint]`:**
1. Fetch notes (best-effort).
2. `checkpoint::resolve_checkpoint(config_path, checkpoint_cfg, ...)`.
3. If resolved: create provider with `.with_checkpoint(ctx)`. Call `prepare()`. If prepare fails (image expired, `git apply` failure), warn, clear checkpoint context, fall through to full build. Update note and push on success.
4. If not resolved: full build (fall through to existing path).

**Without `[checkpoint]`:**
1. Fetch notes (best-effort).
2. `checkpoint::resolve_cached_image(config_path, ...)`.
3. If cached: skip `prepare()`, use cached image ID directly.
4. If not cached: normal `prepare()`, then write note and push.

In both cases, `--no-cache` bypasses all note interactions.

Tests:
- `test_run_with_checkpoint_resolved` (mocks git module, verifies `.with_checkpoint()` called)
- `test_run_with_checkpoint_not_found` (falls through to full build)
- `test_run_with_no_cache_flag` (no note read/write)
- `test_run_without_checkpoint_config_cached` (uses cached image)
- `test_run_without_checkpoint_config_uncached` (builds and caches)

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
1. Load config, require `[checkpoint]` section (else print "Checkpoint mode not configured" and exit).
2. Fetch notes (best-effort).
3. Get HEAD SHA, print it.
4. Walk ancestors looking for nearest checkpoint; print SHA and distance (or "no checkpoint found").
5. If checkpoint found: read note, check for cached image entry.
6. Compute current build inputs hash, compare to cached hash (match/mismatch).
7. Generate diff to determine if thin diff mode will be used.
8. Print summary (see spec for example output).

Tests:
- `test_checkpoint_status_no_config` (no `[checkpoint]` section: prints "not configured")
- `test_checkpoint_status_with_checkpoint` (shows checkpoint info)
- `test_checkpoint_status_no_checkpoint_found` (reports no checkpoint in window)

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
- [ ] First `offload run` against a commit: builds image, writes note, pushes
- [ ] Second `offload run` against same commit: uses cached image from note
- [ ] With `[checkpoint]`: only checkpoint commits trigger full rebuild
- [ ] Non-checkpoint commits after a checkpoint: thin diff applied
- [ ] Untracked files (non-gitignored) included in thin diff tarball
- [ ] `offload run --no-cache`: full build, no note interaction
- [ ] Cached image expired: warns, rebuilds, updates note
- [ ] Build inputs hash mismatch: actionable warning message listing files
- [ ] Missing build_inputs file: error (not silent)
- [ ] Two different TOML configs: separate entries in same note, no collision
- [ ] Note visible and pretty-printed: `git notes --ref=refs/notes/offload-images show HEAD`
- [ ] Note pushed: `git ls-remote origin refs/notes/offload-images`
- [ ] `offload checkpoint-status`: shows checkpoint info, hash status, next run mode
- [ ] Merge commit detected as checkpoint when it touches build_inputs via any parent

## Critical Files

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `CheckpointConfig`, wire into `Config` |
| `src/config.rs` | Validation for checkpoint config |
| `src/git.rs` | **New** -- all git notes operations |
| `src/checkpoint.rs` | **New** -- checkpoint resolution logic |
| `src/lib.rs` | Register `git` and `checkpoint` modules |
| `src/main.rs` | Checkpoint/cache resolution in `run_tests()`, `checkpoint-status` subcommand |
| `src/provider/modal.rs` | `CheckpointContext`, `with_checkpoint()`, modified `build_prepare_command()`, remove `--cached` flag |
| `src/provider/default.rs` | Same checkpoint pattern |
| `scripts/modal_sandbox.py` | Remove `--cached` + cache file functions; add `--from-checkpoint` / `--checkpoint-sha` / `--sandbox-project-root` |
| `skills/offload/SKILL.md` | Checkpoint documentation |
| `skills/offload-onboard/SKILL.md` | Optional checkpoint step |
