# Checkpoint Image Feature for Offload

<Human Problem Specification>

## Problem
Modal image builds are slow, so we want to run them as infrequently as possible, and cache as many steps as we can.

Every `offload run` rebuilds: base image + source overlay + dependency install. The sculptor repo works around this with a manual "keyframe" system. This feature builds that pattern into Offload as a first-class, provider-agnostic capability.

At the same time, existing offload repositories may be small, and do not necessarily need this pattern to achieve significant speed-up.

## Parts of the Design we understand so far

1. Every Offload project maintains a Dockerfile which is used to create the Modal Image. Ideally, this Docker file could
   be agnostic of how the source is uploaded into the image. That is, the same Dockerfile works for checkpointing or direct-copy workflows.

2. The Dockerfile is used to create the base image: OS, programming language dependencies, etc. We want to cache the base image id.

3. For some, small and simple projects, 2 might be sufficient. We could just include the source directories at this point, and then generate the final image.

4. For large projects, we want to put a checkpoint of project source into a checkpoint image. The checkpoint image is also cached.

5. When restoring from a checkpoint image, we have to generate a source diff, place that into the final image, and create the final image by applying the diff.

7. Observation: If we have no checkpoint image, that is homologous to having a checkpoint at no source. There are two ways we can model having no checkpoint image: "The up-to-date source is in the base image" or "The up-to-date source will be loaded later in the diff." No decision has yet been made.

8. How offload handles caching must be robust to expiry at every oint. Modal image caches can expire, and at that point the expired images will need to be rebuilt.

9. We will persist all cache ids in the file system.

</Human Problem Specification>

## Design Summary

- **No TOML config field** -- checkpointing is enabled purely by presence of `.offload/checkpoint-cache`
- **`offload checkpoint`** CLI command creates/updates the checkpoint image
- **`offload checkpoint --delete`** clears the checkpoint
- **`offload run`** detects the cache file and uses a thin `git diff` layer instead of full rebuild
- **Provider-agnostic** -- Modal provider has built-in support; Default provider uses a `checkpoint_command` field; future providers can implement or error
- **Agent skills** updated to know when to call `offload checkpoint`

## Image Architecture

Whether to use checkpoint is determined when `.offload/checkpoint-cache` exists.

### If we are using the checkpoint cache

We deploy a two-layer image architecture:

```
Layer 1: Checkpoint image (base + source + dev + sandbox_init_cmd, cached in .offload/checkpoint-cache)
Layer 2: Run image        (adds a git diff, built fresh each run, cached in .offload-image-cache)
```
- `sandbox_init_cmd` runs ONLY during checkpoint build (not on every run)

### If there is no checkpoint cache:

We have the following image architecture:

```
Layer 1: Cached Image (base + source + dev + sandbox_init_cmd, cached in .offload-image-cache)
```

## Cache File: `.offload/checkpoint-cache`

JSON format:
```json
{
  "sha": "abc123def",
  "image_id": "im-XXXXXXXXX"
}
```
- `sha`: git commit SHA at time of checkpoint creation (immutable once set; only changes via `offload checkpoint`)
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
7. Ensure `.offload/` is in `.gitignore`; create the directory if needed

**Important:** `sandbox_init_cmd` (e.g., `uv sync --all-packages`) is baked into the checkpoint image and does NOT run on subsequent `offload run` invocations. If dependencies change (e.g., `pyproject.toml`, `uv.lock`, `package.json`), `offload checkpoint` must be re-run to rebuild the checkpoint with updated dependencies. Without this, the sandbox will be missing newly added dependencies and tests will fail.

**`--delete` flag:** removes `.offload/checkpoint-cache` and exits.

## Modified `offload run` Flow

When `.offload/checkpoint-cache` exists:
1. Load checkpoint cache (sha + image_id)
2. Load checkpoint image from provider (`modal.Image.from_id(image_id)`)
3. Generate working-tree diff: `git diff <checkpoint-sha> --binary` (no `HEAD` — diffs against the working tree, so uncommitted changes are included, matching the pattern used by sculptor/mng)
4. Collect untracked files via `git ls-files --others --exclude-standard` and package them into a tarball (git diff only captures changes to tracked files; new files that haven't been `git add`'d must be transferred separately)
5. If diff is empty AND no untracked files → use checkpoint image directly (zero overhead)
6. If changes exist → write diff + untracked tarball to temp files, `add_local_file` both into the image, run `git apply` for the diff + extract untracked tarball into `sandbox_project_root`, return run image
7. Skip `include_cwd`, `copy_dirs`, and `sandbox_init_cmd` (all baked into checkpoint)

When `.offload/checkpoint-cache` does NOT exist:
- Existing behavior, completely unchanged

When checkpoint image has expired on Modal:
- Catch exception, warn user visibly
- Rebuild the checkpoint image from the **same SHA**:
  1. Materialize source at the checkpoint SHA via `git archive <sha> | tar -x -C <tmpdir>` (does not touch the working tree)
  2. Build checkpoint image: base + source-from-tmpdir + sandbox_init_cmd
  3. Clean up tmpdir unconditionally (e.g., Python `tempfile.TemporaryDirectory` or `try/finally`)
- Update only the `image_id` in `.offload/checkpoint-cache`; the `sha` is unchanged
- Continue with diff-based run using the newly rebuilt checkpoint

When `--no-cache` is passed to `offload run`:
- Ignore **all** caches: both `.offload/checkpoint-cache` and `.offload-image-cache` (base image cache)
- Full rebuild from scratch: Dockerfile → base image → source overlay → sandbox_init_cmd
- Neither cache file is deleted; they are simply not read. Subsequent runs without `--no-cache` will still use them.

## Prior Refactors

### Provider lifecycle: split `from_config()` into lightweight constructor + `prepare()`

Currently `from_config()` on Modal and Default providers is a heavy async constructor that builds the run image before returning. This couples construction with I/O and makes it impossible to construct a provider without triggering a full image build.

**Change:** Split into two steps on all providers (Modal, Default, Local):
- `from_config(config, connector)` — lightweight, synchronous. Stores config, connector, command templates. No I/O.
- `prepare(no_cache, checkpoint_cache) -> ProviderResult<String>` — new trait method. Runs the image build (existing logic moved here), returns image_id.

**Calling code changes in `main.rs`:**
```
// Before:
let provider = ModalProvider::from_config(config, connector, discovery_done).await?;

// After:
let provider = ModalProvider::from_config(config, connector);
let image_id = provider.prepare(no_cache, checkpoint_cache).await?;
```

**Why this is a prerequisite:** The checkpoint feature needs to construct a provider without running `prepare()` (for `offload checkpoint`, which calls `build_checkpoint()` instead). Without this split, there is no way to get a provider instance without also building a run image.

**This refactor is behavior-preserving.** Existing `offload run` calls `from_config()` then `prepare()` in sequence and gets the same result as today. It should land as its own commit before checkpoint work begins.

## Files to Modify

### 1. `src/main.rs` -- Add `Checkpoint` subcommand

- Add `Checkpoint` variant to `Commands` enum (~line 48) with `--delete` and `--no-cache` flags
- Add dispatch to `checkpoint_handler()` in the match block (~line 145)
- `checkpoint_handler()`:
  - If `--delete`: remove `.offload/checkpoint-cache`, exit
  - Otherwise: load config, construct provider via `from_config()` (lightweight), call `provider.build_checkpoint()`, write cache file
- Update `offload run` dispatch to use new provider lifecycle (see Prior Refactors)

### 2. `src/config/schema.rs` -- Add `checkpoint_command` to DefaultProviderConfig

- Add `checkpoint_command: Option<String>` to `DefaultProviderConfig` (~line 206)
- No changes to `ModalProviderConfig` (Modal checkpoint is handled internally by modal_sandbox.py)

### 3. `src/provider.rs` -- Add checkpoint to provider trait

After the prior refactor, the trait has `prepare()`. Add one new method:
```rust
async fn build_checkpoint(&self, no_cache: bool, source_dir: Option<&Path>) -> ProviderResult<String>;
```

The trait now has two image-building methods:
- `prepare()` — builds the run image (called by `offload run`; checkpoint-aware when cache exists)
- `build_checkpoint()` — builds the checkpoint image (called by `offload checkpoint`)

### 4. `src/provider/modal.rs` -- Checkpoint support

**`prepare()` changes** (after prior refactor moves existing logic here):
- When `checkpoint_cache` is provided: append `--from-checkpoint=<image_id> --checkpoint-sha=<sha> --sandbox-project-root=<root>` to the prepare command instead of `--include-cwd`/`--copy-dir`/`--sandbox-init-cmd`

**`build_checkpoint()` implementation:**
- Calls `uv run @modal_sandbox.py checkpoint [dockerfile] [flags]`
- Appends standard flags: `--include-cwd`, `--copy-dir=...`, `--sandbox-init-cmd=...`, `--cached`
- If `source_dir` is provided (expiry recovery): appends `--source-dir=<path>`

### 5. `src/provider/default.rs` -- Checkpoint support

**`prepare()` changes** (after prior refactor moves existing logic here):
- When `checkpoint_cache` is provided: append `--from-checkpoint=<image_id> --checkpoint-sha=<sha> --sandbox-project-root=<root>` to `prepare_command` (same flags as Modal provider)

**`build_checkpoint()` implementation:**
- Runs `checkpoint_command` if defined in config
- Appends the same standard flags as Modal: `--include-cwd`, `--copy-dir=...`, `--sandbox-init-cmd=...`, `--cached`, `--source-dir=<path>`
- If `checkpoint_command` is not set: error with "Set `checkpoint_command` in your provider config to enable checkpointing"

Both providers follow the same flag-appending protocol and trait interface, ensuring Liskov substitutability of `SandboxProvider`.

### 6. `scripts/modal_sandbox.py` -- Core implementation

**New `checkpoint` CLI command:**
```
uv run @modal_sandbox.py checkpoint [dockerfile] [--include-cwd] [--copy-dir=...] [--sandbox-init-cmd=...] [--cached] [--source-dir=<path>]
```
- Builds base + source + init (same as current `prepare` with full overlay)
- `--source-dir`: if provided, use this directory as the source root instead of cwd (used during expiry recovery with `git archive` output)
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
2. Generate working-tree diff: `git diff <checkpoint-sha> --binary` (includes uncommitted changes)
3. Collect untracked files: `git ls-files --others --exclude-standard`, package into tarball
4. If diff is empty AND no untracked files → return checkpoint image_id (zero overhead)
5. Write diff + untracked tarball to temp files → `add_local_file` both → `git apply` diff + extract tarball into project root → return run image_id
6. Skip `--include-cwd`, `--copy-dir`, `--sandbox-init-cmd` (baked into checkpoint)

**New functions:**
- `_generate_git_diff(sha)` -- runs `git diff <sha> --binary` (working-tree diff), returns string or None
- `_collect_untracked_files()` -- runs `git ls-files --others --exclude-standard`, packages results into tarball, returns path or None
- `_build_run_image_from_checkpoint(app, checkpoint_img, checkpoint_img_id, diff, untracked_tar, project_root)` -- applies diff via `git apply` + extracts untracked tarball

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
| Checkpoint image expired on Modal | Warn user, rebuild checkpoint from same SHA, update image_id only |
| New untracked files since checkpoint | Detected via `git ls-files --others --exclude-standard`, included alongside diff |
| `offload run --no-cache` | Ignore all caches (checkpoint + base image), full rebuild from scratch |
| Empty diff (no changes since checkpoint) | Return checkpoint image_id directly (zero overhead) |
| Binary files in diff | `git diff --binary` + `git apply` handles them |
| Large diff | `add_local_file` avoids shell argument limits |
| Dependencies changed since checkpoint | Tests fail with missing deps; user must re-run `offload checkpoint` |
| Default provider without `checkpoint_command` | `offload checkpoint` errors with clear message |
| `offload checkpoint --delete` | Remove `.offload/checkpoint-cache`, exit |
| `.offload/` dir doesn't exist | Create it on first `offload checkpoint` |

## Implementation Order

1. **Provider lifecycle refactor** -- See Prior Refactors section. Pure refactor, no behavior change.
2. **Rust CLI skeleton** -- Add `Checkpoint` subcommand to `Commands` enum, `--delete` flag, cache file read/write in `.offload/checkpoint-cache`
3. **Provider trait** -- Add `build_checkpoint()` method to `SandboxProvider` trait
4. **Modal provider** -- Implement `build_checkpoint()`, update `prepare()` to handle checkpoint cache
5. **Default provider** -- Implement `build_checkpoint()` using `checkpoint_command` config field, update `prepare()` for checkpoint-aware flags
6. **Python `checkpoint` command** -- New CLI command in modal_sandbox.py that builds full checkpoint image
7. **Python `prepare` modifications** -- Add `--from-checkpoint`, `--checkpoint-sha`, `--sandbox-project-root` options; implement diff-based run image building
8. **Agent skills** -- Update `skills/offload/SKILL.md` and `skills/offload-onboard/SKILL.md`
9. **Tests** -- Rust unit tests for config parsing, command building, cache file I/O; verify provider refactor doesn't regress existing behavior

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
