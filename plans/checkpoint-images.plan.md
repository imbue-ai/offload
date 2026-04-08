# Checkpoint Images -- Implementation Plan

Implementation plan for the checkpoint images spec (`checkpoint-images.spec.md`).

## Key Technical Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Git operations | Shell out to `git` CLI | No `git2` crate (heavy native dep); works with jj colocated repos; simple operations only |
| Identity hashing | `git hash-object --stdin` (SHA-1) | Already available; avoids new `sha2` crate; sufficient for change detection |
| Notes content | Single-line JSON | Need both `image_id` and `identity_hash` per note |
| `.offload-image-cache` | Keep as-is in `modal_sandbox.py` | Caches Dockerfile base image locally; different concern from checkpoint notes |

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
    pub image_identity: Vec<String>,
}
```

Add to `Config`:
```rust
#[serde(default)]
pub checkpoint: Option<CheckpointConfig>,
```

Validation (in `config.rs`): if `checkpoint` is `Some`, `image_identity` must be non-empty; provider must not be `Local`.

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
pub struct CheckpointNote {
    pub image_id: String,
    pub identity_hash: String,
}

pub fn head_sha() -> Result<String>;
pub fn repo_root() -> Result<PathBuf>;
pub fn compute_identity_hash(repo_root: &Path, files: &[String]) -> Result<String>;
pub fn read_note(commit_sha: &str) -> Result<Option<CheckpointNote>>;
pub fn write_note(commit_sha: &str, note: &CheckpointNote) -> Result<()>;
pub fn remove_note(commit_sha: &str) -> Result<()>;
pub fn push_notes(remote: &str) -> Result<()>;
pub fn fetch_notes(remote: &str) -> Result<()>;
pub fn configure_notes_fetch(remote: &str) -> Result<()>;
pub fn find_nearest_checkpoint(max_depth: usize) -> Result<Option<(String, CheckpointNote)>>;
pub fn is_working_tree_clean() -> Result<bool>;
```

Implementation notes:
- `compute_identity_hash`: sort files lexicographically; for each, prepend `FILE:<path>:<len>\n` header; concatenate all; pipe to `git hash-object --stdin`.
- `find_nearest_checkpoint`: run `git notes --ref=refs/notes/offload-images list` to get all annotated SHAs, then `git log --format=%H -n <max_depth>` and intersect. Avoids O(n) note lookups.
- `configure_notes_fetch`: check `git config --get-all remote.<remote>.fetch` for existing refspec; add `+refs/notes/offload-images:refs/notes/offload-images` if absent.
- `is_working_tree_clean`: `git status --porcelain` and check output is empty.

Tests (unit):
- `test_checkpoint_note_json_round_trip`
- `test_identity_hash_deterministic` (write temp files, call twice, assert equal)
- `test_identity_hash_content_sensitive` (change a file, assert different hash)

Tests (integration, create temp git repos):
- `test_write_and_read_note`
- `test_find_nearest_checkpoint_with_note`
- `test_find_nearest_checkpoint_no_notes`
- `test_configure_notes_fetch_idempotent`
- `test_remove_note`

---

### Commit 3: `offload checkpoint` CLI subcommand

**Files:** `src/main.rs`

Add to `Commands` enum:
```rust
Checkpoint {
    #[arg(long)]
    no_cache: bool,
    #[arg(long)]
    delete: bool,
    #[arg(long, default_value = "origin")]
    remote: String,
},
```

Implement `checkpoint_handler()`:
1. Load config, require `[checkpoint]` section.
2. If `--delete`: `git::remove_note(&git::head_sha()?)`, exit.
3. Verify `git::is_working_tree_clean()`.
4. `git::fetch_notes(remote)` (best-effort).
5. Get HEAD SHA, compute identity hash.
6. Build checkpoint image via provider `prepare()`.
7. `git::write_note(&sha, &note)`.
8. `git::configure_notes_fetch(remote)` (idempotent).
9. `git::push_notes(remote)`.

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

Apply same pattern to `DefaultProvider` (append flags to `prepare_command`).

Tests:
- `test_prepare_command_with_checkpoint`
- `test_prepare_command_without_checkpoint` (existing tests still pass)

---

### Commit 5: Modified `offload run` flow

**Files:** `src/main.rs`

In `run_tests()`, after loading config and before provider dispatch:

```
if config.checkpoint is Some:
    fetch_notes (best-effort)
    find_nearest_checkpoint(100)
    if found:
        compute current identity hash
        if hashes match:
            set checkpoint_info = Some(...)
        else:
            warn "identity files changed"
    else:
        info "no checkpoint found"
```

In the Modal/Default provider creation blocks, if `checkpoint_info` is `Some`:
```
provider = provider.with_checkpoint(CheckpointContext { ... })
```

---

### Commit 6: Python `prepare` modifications

**Files:** `scripts/modal_sandbox.py`

Add options to `prepare` command:
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
5. On image-expired exception: warn, clear, fall back to full build

New helpers:
- `_generate_git_diff(checkpoint_sha: str) -> str | None`
- `_build_run_image_from_checkpoint(app, checkpoint_img, diff: str, project_root: str) -> str`

---

### Commit 7: Agent skills updates

**Files:** `skills/offload/SKILL.md`, `skills/offload-onboard/SKILL.md`

- Add checkpoint section to offload skill (when to checkpoint, CLI reference)
- Add optional checkpoint setup step to onboarding skill
- Update troubleshooting tables

---

### Commit 8: Integration tests + cleanup

- Manual verification against Modal
- Remove the old `#checkpoint-images.md#` draft plan file

## Verification Checklist

Automated (every commit):
- [ ] `cargo fmt --check`
- [ ] `cargo clippy` (no warnings)
- [ ] `cargo nextest run`

Manual (after all commits, requires Modal credentials):
- [ ] `offload checkpoint` creates note on HEAD (`git notes --ref=refs/notes/offload-images show HEAD`)
- [ ] `offload run` with checkpoint uses thin diff (logs show `[checkpoint] Using checkpoint from ...`)
- [ ] `offload run --no-cache` ignores checkpoint
- [ ] Edit a file in `image_identity` → `offload run` warns and falls back
- [ ] `offload checkpoint --delete` removes note
- [ ] No source changes since checkpoint → checkpoint image used directly
- [ ] Note pushed to remote (`git ls-remote origin refs/notes/offload-images`)

## Critical Files

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `CheckpointConfig`, wire into `Config` |
| `src/config.rs` | Validation for checkpoint config |
| `src/git.rs` | **New** -- all git notes operations |
| `src/lib.rs` | Register `git` module |
| `src/main.rs` | `Checkpoint` subcommand, checkpoint resolution in `run_tests()` |
| `src/provider/modal.rs` | `CheckpointContext`, `with_checkpoint()`, modified `build_prepare_command()` |
| `src/provider/default.rs` | Same checkpoint pattern |
| `scripts/modal_sandbox.py` | `--from-checkpoint` / `--checkpoint-sha` / `--sandbox-project-root` |
| `skills/offload/SKILL.md` | Checkpoint documentation |
| `skills/offload-onboard/SKILL.md` | Optional checkpoint step |
