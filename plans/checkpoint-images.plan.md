# Checkpoint Images -- Implementation Plan

Implementation plan for the checkpoint images spec (`checkpoint-images.spec.md`).

## Key Technical Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Git operations | Shell out to `git` CLI | No `git2` crate (heavy native dep); works with jj colocated repos; simple operations only |
| Build inputs hashing | `git hash-object --stdin` (SHA-1) | Already available; avoids new `sha2` crate; sufficient for change detection |
| Notes content | JSON keyed by TOML config path | Prevents collision when multiple configs target different Dockerfiles |
| `.offload-image-cache` | Remove from `modal_sandbox.py` | Git notes are the sole caching mechanism; `.offload-image-cache` is superseded |
| Checkpoint detection | `git diff --name-only <parent> <commit>` | Pure function of commit content; no manual step required |

## Commit Sequence

Seven atomic commits. Each must pass `cargo fmt --check`, `cargo clippy`, `cargo nextest run`.

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

New module encapsulating all git interactions. All functions use `std::process::Command` (blocking).

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

pub fn head_sha() -> Result<String>;
pub fn repo_root() -> Result<PathBuf>;
pub fn compute_build_inputs_hash(repo_root: &Path, files: &[String]) -> Result<String>;
pub fn read_note(commit_sha: &str) -> Result<Option<NoteContents>>;
pub fn write_note(commit_sha: &str, contents: &NoteContents) -> Result<()>;
pub fn push_notes(remote: &str) -> Result<()>;
pub fn fetch_notes(remote: &str) -> Result<()>;
pub fn configure_notes_fetch(remote: &str) -> Result<()>;
pub fn commit_touches_paths(commit_sha: &str, paths: &[String]) -> Result<bool>;
pub fn ancestors(max_depth: usize) -> Result<Vec<String>>;
```

Implementation notes:
- `compute_build_inputs_hash`: sort files lexicographically; for each, prepend `FILE:<path>:<len>\n` header; concatenate all; pipe to `git hash-object --stdin`.
- `commit_touches_paths`: `git diff --name-only <sha>^..<sha>`, intersect with `paths`.
- `ancestors`: `git log --format=%H -n <max_depth>`.
- `read_note` / `write_note`: read/write full JSON object. `write_note` does a read-modify-write to merge entries (so two configs don't clobber each other). Concurrency policy is last write wins: `git notes add -f` overwrites unconditionally, and `push_notes` uses force-push. Redundant rebuilds from lost writes are acceptable.
- `configure_notes_fetch`: check `git config --get-all remote.<remote>.fetch` for existing refspec; add `+refs/notes/offload-images:refs/notes/offload-images` if absent.

Tests (unit):
- `test_image_entry_json_round_trip`
- `test_build_inputs_hash_deterministic` (write temp files, call twice, assert equal)
- `test_build_inputs_hash_content_sensitive` (change a file, assert different hash)

Tests (integration, create temp git repos):
- `test_write_and_read_note`
- `test_write_note_merges_configs`
- `test_commit_touches_paths`
- `test_configure_notes_fetch_idempotent`

---

### Commit 3: Checkpoint resolution logic -- `src/checkpoint.rs`

**Files:** `src/checkpoint.rs` (new), `src/lib.rs`

High-level checkpoint resolution, called from `offload run`. Depends on `src/git.rs`.

```rust
pub struct ResolvedCheckpoint {
    pub checkpoint_sha: String,
    pub image_id: String,
    pub sandbox_project_root: String,
}

/// Find the nearest checkpoint ancestor, returning its SHA and cached image
/// (building and caching if needed).
pub async fn resolve_checkpoint(
    config_path: &str,
    checkpoint_cfg: &CheckpointConfig,
    provider: &mut impl BuildableProvider,  // trait for "can build a full image"
    max_depth: usize,
) -> Result<Option<ResolvedCheckpoint>>;

/// For non-checkpoint mode: check if HEAD has a cached image, build if not.
pub async fn resolve_cached_image(
    config_path: &str,
    provider: &mut impl BuildableProvider,
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

### Commit 5: Integrate into `offload run`

**Files:** `src/main.rs`

In `run_tests()`, after loading config and before provider dispatch:

**With `[checkpoint]`:**
1. Fetch notes (best-effort).
2. `checkpoint::resolve_checkpoint(config_path, checkpoint_cfg, ...)`.
3. If resolved: create provider with `.with_checkpoint(ctx)`.
4. If not resolved: full build (fall through to existing path).

**Without `[checkpoint]`:**
1. Fetch notes (best-effort).
2. `checkpoint::resolve_cached_image(config_path, ...)`.
3. If cached: skip `prepare()`, use cached image ID directly.
4. If not cached: normal `prepare()`, then write note and push.

In both cases, `--no-cache` bypasses all note interactions.

---

### Commit 6: Python `prepare` modifications

**Files:** `scripts/modal_sandbox.py`

**Remove old caching mechanism:**
- Delete `CACHE_FILE = ".offload-image-cache"`
- Delete `read_cached_image_id()`, `write_cached_image_id()`, `clear_image_cache()`
- Remove `--cached` flag from `prepare` command
- Remove all call sites that read/write/clear the cache file

Image caching is now handled entirely by the Rust side via git notes. The Python script is stateless: it builds images and returns IDs.

**Add checkpoint options to `prepare` command:**
- `--from-checkpoint` (image ID string)
- `--checkpoint-sha` (git SHA string)
- `--sandbox-project-root` (default `/app`)

When `--from-checkpoint` is set:
1. `modal.Image.from_id(from_checkpoint)`
2. `subprocess.run(["git", "diff", checkpoint_sha, "HEAD", "--binary"])` locally
3. If empty: return checkpoint image ID (print to stdout)
4. If non-empty:
   - Write diff to temp file
   - `checkpoint_img.add_local_file(diff_path, "/tmp/offload.patch")`
   - `.run_commands(f"cd {project_root} && git apply /tmp/offload.patch --allow-empty && rm /tmp/offload.patch")`
   - Build, materialize, return new image ID
5. On image-expired exception: warn, fall back to full build

New helpers:
- `_generate_git_diff(checkpoint_sha: str) -> str | None`
- `_build_run_image_from_checkpoint(app, checkpoint_img, diff: str, project_root: str) -> str`

---

### Commit 7: Agent skills updates + cleanup

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
- [ ] `offload run --no-cache`: full build, no note interaction
- [ ] Cached image expired: warns, rebuilds, updates note
- [ ] Two different TOML configs: separate entries in same note, no collision
- [ ] Note visible: `git notes --ref=refs/notes/offload-images show HEAD`
- [ ] Note pushed: `git ls-remote origin refs/notes/offload-images`

## Critical Files

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `CheckpointConfig`, wire into `Config` |
| `src/config.rs` | Validation for checkpoint config |
| `src/git.rs` | **New** -- all git notes operations |
| `src/checkpoint.rs` | **New** -- checkpoint resolution logic |
| `src/lib.rs` | Register `git` and `checkpoint` modules |
| `src/main.rs` | Checkpoint/cache resolution in `run_tests()` |
| `src/provider/modal.rs` | `CheckpointContext`, `with_checkpoint()`, modified `build_prepare_command()`, remove `--cached` flag |
| `src/provider/default.rs` | Same checkpoint pattern |
| `scripts/modal_sandbox.py` | Remove `--cached` + cache file functions; add `--from-checkpoint` / `--checkpoint-sha` / `--sandbox-project-root` |
| `skills/offload/SKILL.md` | Checkpoint documentation |
| `skills/offload-onboard/SKILL.md` | Optional checkpoint step |
