# Checkpoint Image Feature for Offload

## Problem
Modal image builds are slow. Every `offload run` rebuilds: base image + source overlay + dependency install. The sculptor repo works around this with a manual keyframe system. This feature builds that pattern into Offload as a first-class, provider-agnostic capability.

## Design Summary

- **No TOML config field** -- checkpointing is enabled purely by presence of `.offload/checkpoint-cache`
- **`offload checkpoint`** CLI command creates/updates the checkpoint image
- **`offload checkpoint --delete`** clears the checkpoint
- **`offload run`** detects the cache file and uses a thin `git diff` layer instead of full rebuild
- **Provider-agnostic** -- Modal provider has built-in support; Default provider uses a `checkpoint_command` field; future providers can implement or error
- **Agent skills** updated to know when to call `offload checkpoint`

## Three-Layer Image Architecture

```
Layer 1: Base image       (from Dockerfile, cached in .offload-image-cache)
Layer 2: Checkpoint image (base + source + sandbox_init_cmd, cached in .offload/checkpoint-cache)
Layer 3: Run image        (checkpoint + git diff, built fresh each run)
```

- `sandbox_init_cmd` runs ONLY during checkpoint build (not on every run)
- When no checkpoint exists, current behavior is preserved (layers 1+3 collapse to existing flow)

## Cache File: `.offload/checkpoint-cache`

JSON format:
```json
{
  "version": 1,
  "sha": "abc123def",
  "image_id": "im-XXXXXXXXX"
}
```
- `sha`: git commit SHA at time of checkpoint creation
- `image_id`: Modal/provider image ID for the checkpoint
- File presence = checkpointing enabled for `offload run`
- `.offload/` directory should be in `.gitignore`

## CLI: `offload checkpoint`

New subcommand added to the `Commands` enum:

```
offload checkpoint              # Build checkpoint image from current state
offload checkpoint --delete     # Clear checkpoint cache
offload checkpoint --no-cache   # Force fresh base image rebuild too
```

**What it does:**
1. Check `git status --porcelain` -- if non-empty, refuse with "commit your changes before checkpointing"
   - Works identically in pure git and jj-colocated repos (no VCS-specific handling)
   - In jj, this means `@` must be empty (changes are in `@-` or earlier)
2. Record SHA via `git rev-parse HEAD` (in jj-colocated repos, this is `@-`, which is immutable)
3. Load config (same as `offload run`)
4. Build/load cached base image (Dockerfile)
5. Build checkpoint image: base + `include_cwd` + `copy_dirs` + `sandbox_init_cmd`
6. Write `{"sha": "<sha>", "image_id": "<id>"}` to `.offload/checkpoint-cache`

**`--delete` flag:** removes `.offload/checkpoint-cache` and exits.

## Modified `offload run` Flow

When `.offload/checkpoint-cache` exists:
1. Load checkpoint cache (sha + image_id)
2. Load checkpoint image from provider (`modal.Image.from_id(image_id)`)
3. Generate `git diff <checkpoint-sha> --binary` to capture tracked file changes since checkpoint
4. Collect untracked files via `git ls-files --others --exclude-standard` and include them (git diff alone misses new files not yet `git add`'d)
5. If diff is empty AND no untracked files → use checkpoint image directly (zero overhead)
6. If changes exist → write diff to temp file, `add_local_file` for diff + any untracked files, `git apply` + copy untracked files, return run image
7. Skip `include_cwd`, `copy_dirs`, and `sandbox_init_cmd` (all baked into checkpoint)

When `.offload/checkpoint-cache` does NOT exist:
- Existing behavior, completely unchanged

When cached image has expired on Modal:
- Catch exception, warn user visibly, fall back to full build, and UPDATE the cache with the newly built image (so subsequent runs benefit without manual intervention)

When `--no-cache` is passed to `offload run`:
- Ignore checkpoint cache, do full build (existing behavior)

## Files to Modify

### 1. `src/main.rs` -- Add `Checkpoint` subcommand

- Add `Checkpoint` variant to `Commands` enum (~line 48) with `--delete` and `--no-cache` flags
- Add dispatch to `checkpoint_handler()` in the match block (~line 145)
- `checkpoint_handler()`:
  - If `--delete`: remove `.offload/checkpoint-cache`, exit
  - Otherwise: load config, build checkpoint image via provider, write cache file
- Update `ModalProvider::from_config` call site (~line 527) to pass `&config.offload.sandbox_project_root`

### 2. `src/config/schema.rs` -- Add `checkpoint_command` to DefaultProviderConfig

- Add `checkpoint_command: Option<String>` to `DefaultProviderConfig` (~line 206)
- No changes to `ModalProviderConfig` (Modal checkpoint is handled internally by modal_sandbox.py)

### 3. `src/provider.rs` -- Add checkpoint to provider trait

- Add `build_checkpoint()` method to the `SandboxProvider` trait:
  ```rust
  async fn build_checkpoint(&self) -> ProviderResult<String>;  // returns image_id
  ```
- Add `run_checkpoint_command()` helper (similar to `run_prepare_command()`)

### 4. `src/provider/modal.rs` -- Checkpoint support

- Add `build_checkpoint()` implementation: calls `uv run @modal_sandbox.py checkpoint`
- Modify `from_config()` to accept `sandbox_project_root` parameter
- When checkpoint cache exists during `from_config()`, pass `--from-checkpoint=<image_id> --checkpoint-sha=<sha> --sandbox-project-root=<root>` to the prepare command

### 5. `src/provider/default.rs` -- Checkpoint support

- Add `build_checkpoint()` implementation: runs `checkpoint_command` if defined, errors if not
- Pass through copy_dirs and sandbox_init_cmd to checkpoint_command

### 6. `scripts/modal_sandbox.py` -- Core implementation

**New `checkpoint` CLI command:**
```
uv run @modal_sandbox.py checkpoint [dockerfile] [--include-cwd] [--copy-dir=...] [--sandbox-init-cmd=...] [--cached]
```
- Builds base + source + init (same as current `prepare` with full overlay)
- Outputs image_id to stdout
- The Rust side captures this and writes `.offload/checkpoint-cache`

**Modified `prepare` command -- new options:**
```
--from-checkpoint=<image_id>     # Use this checkpoint as starting point
--checkpoint-sha=<sha>           # Git SHA of the checkpoint (for diff generation)
--sandbox-project-root=<path>    # Where to apply the diff (default: /app)
```

When `--from-checkpoint` is set:
1. Load checkpoint image via `modal.Image.from_id(image_id)`
2. Run `git diff <checkpoint-sha> --binary`
3. If empty → return checkpoint image_id
4. Write diff to temp file → `add_local_file` → `git apply` → return run image_id
5. Skip `--include-cwd`, `--copy-dir`, `--sandbox-init-cmd` (baked into checkpoint)

**New functions:**
- `_generate_git_diff(sha)` -- runs `git diff <sha> --binary`, returns string or None
- `_build_run_image_from_checkpoint(app, checkpoint_img, checkpoint_img_id, diff, project_root)` -- applies diff via `add_local_file` + `git apply`

**Cache management stays in Rust** (not Python) -- the `.offload/checkpoint-cache` file is read/written by the Rust CLI, not by modal_sandbox.py. This keeps the Python script stateless.

### 7. `skills/offload/SKILL.md` -- Update agent skill

Add section on checkpointing:
- When to run `offload checkpoint` (after changing deps, lock files, Dockerfile)
- How to check if checkpoint exists
- When to use `offload checkpoint --delete` (troubleshooting stale images)
- Add `offload checkpoint` to CLI Quick Reference table

### 8. `skills/offload-onboard/SKILL.md` -- Update onboarding skill

- Add optional step for setting up initial checkpoint after onboarding
- Mention `offload checkpoint` as optimization after Step 10 (parallelism tuning)
- Update troubleshooting table with checkpoint-related entries

## Edge Cases

| Case | Behavior |
|------|----------|
| First run, no checkpoint | Current behavior unchanged |
| After `offload checkpoint` | `offload run` uses thin diff layer |
| Checkpoint image expired on Modal | Warn user, fall back to full build, update cache with new image |
| New untracked files since checkpoint | Detected via `git ls-files --others --exclude-standard`, included alongside diff |
| `offload run --no-cache` | Ignore checkpoint cache, full build |
| Empty diff (no changes since checkpoint) | Return checkpoint image_id directly (zero overhead) |
| Binary files in diff | `git diff --binary` + `git apply` handles them |
| Large diff | `add_local_file` avoids shell argument limits |
| Default provider without `checkpoint_command` | `offload checkpoint` errors with clear message |
| `offload checkpoint --delete` | Remove `.offload/checkpoint-cache`, exit |
| `.offload/` dir doesn't exist | Create it on first `offload checkpoint` |

## Implementation Order

1. **Rust CLI skeleton** -- Add `Checkpoint` subcommand to `Commands` enum, `--delete` flag, cache file read/write in `.offload/checkpoint-cache`
2. **Provider trait** -- Add `build_checkpoint()` method or `CheckpointProvider` trait
3. **Modal provider** -- Implement `build_checkpoint()`, modify `from_config()` to handle checkpoint cache
4. **Default provider** -- Implement `build_checkpoint()` using `checkpoint_command` config field
5. **Python `checkpoint` command** -- New CLI command in modal_sandbox.py that builds full checkpoint image
6. **Python `prepare` modifications** -- Add `--from-checkpoint`, `--checkpoint-sha`, `--sandbox-project-root` options; implement diff-based run image building
7. **Agent skills** -- Update `skills/offload/SKILL.md` and `skills/offload-onboard/SKILL.md`
8. **Tests** -- Rust unit tests for config parsing, command building, cache file I/O

## Verification

1. `cargo fmt --check` passes
2. `cargo clippy` passes (no warnings)
3. `cargo nextest run` passes
4. Manual test: `offload checkpoint` creates `.offload/checkpoint-cache` with valid JSON
5. Manual test: `offload run` after checkpoint uses diff-based image (visible in logs)
6. Manual test: `offload run --no-cache` ignores checkpoint
7. Manual test: `offload checkpoint --delete` removes cache file
8. Manual test: changing code after checkpoint → diff applied correctly
9. Manual test: no changes after checkpoint → checkpoint image used directly
