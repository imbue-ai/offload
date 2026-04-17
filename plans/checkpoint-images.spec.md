# Checkpoint Images -- Specification

This document specifies the requirements for checkpoint image support in Offload.
It supersedes the earlier draft in `#checkpoint-images.md#`.

## Problem

Modal image builds are slow. Every `offload run` rebuilds: base image + source overlay + dependency install. The sculptor and mngr repos work around this with manual "keyframe" systems using committed files (`.offload-base-commit`, `.offload-image-cache`). This is clunky: the files require their own commits and are easy to forget to update.

This feature builds checkpoints into Offload as a first-class capability, using git notes instead of committed files for metadata storage.

## Goals

1. Make image selection/building for sandboxed test runs Just Work for users.
2. Avoid rebuilding expensive dependency/download layers on every commit.
3. Keep a single source of truth for commit → image mapping.
4. Store metadata without mutating commit SHAs.
5. Support both Git and jj users with the same backend mechanism.
6. Keep the system easy to bootstrap via `offload init`.

## Non-Goals

- Automatically copying metadata across rebases/amends (new commits get their own mapping).
- Replacing Modal's internal Dockerfile layer caching (that remains as-is).

## Future Work

- **Configurable `max_depth`**: The ancestor walk currently uses a hardcoded depth limit. Once the walk exceeds this limit without finding a checkpoint, every subsequent commit is treated as needing a full build. Making `max_depth` configurable (or adaptive) is deferred to a follow-up.

## Definitions

**Checkpoint**: A commit whose diff (relative to **any** of its parents) modifies any file in the `build_inputs` set. For merge commits (including octopus merges), the diff is checked against each parent separately using `git diff-tree --no-commit-id --name-only -r -m <sha>`. The `-m` flag produces one diff per parent; if any of those diffs touches a `build_inputs` file, the merge is a checkpoint. Whether a commit is a checkpoint is a pure function of the commit's content and the config -- it is not a manual designation.

**Checkpoint image**: A full image built from a checkpoint commit (Dockerfile base + source checkout + dependency install + sandbox_init_cmd). Expensive to build. Cached in git notes.

**Base image (latest-commit caching)**: When `[checkpoint]` is absent (the default), the image built from the latest commit (HEAD) serves as the base image for subsequent runs. The current run applies a thin diff of uncommitted changes on top of this base. Cached in git notes on the HEAD commit.

## Two-Image Model

When `[checkpoint]` is configured:

```
Checkpoint image (rebuilt infrequently):
  Dockerfile base + source checkout + dependency install + sandbox_init_cmd
  Built from a checkpoint commit. Cached in git notes on that commit.

Current image (rebuilt each run):
  Checkpoint image + thin git diff of changes since checkpoint
```

When `[checkpoint]` is absent:

```
Base image (rebuilt when HEAD changes):
  Dockerfile base + source checkout + sandbox_init_cmd
  Built from the latest commit tree (exported via git, same as checkpoint).
  Cached in git notes on that commit.

Current image (rebuilt each run):
  Base image + thin diff of uncommitted changes since HEAD
```

In both cases, git notes are the storage mechanism. The first run against a commit that lacks a cached image builds and caches it automatically. There is no manual "create checkpoint" step.

**Requirement**: Checkpoint mode uses `git apply` inside the sandbox to apply thin diffs. The Dockerfile must install `git` in the image. If `git` is not available, `git apply` will fail and offload will fall back to a full build with a warning.

## Configuration: `[checkpoint]` Section

A new optional TOML section in `offload.toml`:

```toml
[checkpoint]
build_inputs = [
    "Dockerfile",
    "requirements.txt",
    "setup.py",
    "pyproject.toml",
]
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `build_inputs` | `list[str]` | Yes (if section present) | Repo-relative file paths. A commit that modifies any of these files is a checkpoint. |

### Semantics

- If the `[checkpoint]` section is absent, checkpoints do not exist. Offload uses the latest commit (HEAD) as the base: it exports the HEAD tree, builds from it, caches the result, and applies a thin diff of uncommitted changes.
- If the section is present, `build_inputs` must be non-empty.
- The listed files are the "build spec" -- Dockerfiles, dependency manifests, build/install scripts.
- A commit is a checkpoint if and only if its diff versus **any** parent touches any file in this list. For merge commits, each parent is checked independently; a difference against any single parent is sufficient.

## Metadata Storage: Git Notes

### Design

- Notes ref: `refs/notes/offload-images`
- Key: Git commit SHA
- Value: JSON object keyed by TOML config file path (repo-relative, no `./` prefix, e.g. `offload.toml` not `./offload.toml`)

Example note on a commit (two configs in the same repo):

```json
{
  "offload-modal.toml": {
    "image_id": "im-abc123"
  },
  "offload-integration.toml": {
    "image_id": "im-def456"
  }
}
```

This structure ensures that multiple TOML configs (with potentially different Dockerfiles) never collide in the same note.

Config paths are canonicalized to repo-relative paths with no `./` prefix before use as keys. This prevents `./offload.toml` and `offload.toml` from creating separate entries.

JSON in notes is pretty-printed (indented) for human debuggability. Users can inspect notes directly via `git notes --ref=refs/notes/offload-images show HEAD`.

### Properties

- Attaches metadata to a commit without changing the commit SHA.
- The notes ref is stored on GitHub (or other remote) but is not visible in normal GitHub UI.
- Users interact with it exclusively through `offload` commands, never directly.
- `offload` explicitly reads, writes, fetches, and pushes notes.
- `offload init` (or first `offload run`) configures the local repo to auto-fetch the notes ref.

### Concurrency

Git notes are a **write-through cache**. The expensive work is the image build itself, which has already completed by the time a note is written. Losing a note entry is cheap -- it simply means the next run that needs that entry rebuilds and re-caches it.

Multiple `offload run` invocations (e.g. parallel CI jobs) may attempt to write notes to the same commit simultaneously. The concurrency policy is **last write wins**:

1. Read-modify-write the note JSON locally (merge entries so two configs targeting the same commit don't clobber each other within one writer).
2. Force-push the notes ref to the remote.

A concurrent writer may clobber another's entry. This is acceptable: image builds are idempotent, and the "lost" entry is simply rebuilt on the next cache miss. No locking protocol is needed.

### jj Support

- Offload operates on the underlying colocated Git repo (`.git/` directory).
- The mapping is keyed by Git commit SHA, not jj change ID.
- Metadata is NOT automatically copied across rebases or amends. If history is rewritten, the new commit has no note; the first `offload run` against it will build and cache a new image.

## `offload run` Flow

Test discovery runs once concurrently with the entire prewarm pipeline (via `tokio::try_join!`). The prewarm pipeline is invoked through `provider.prewarm_image_cache()` which delegates to `image_cache::run_prewarm_pipeline()`.

### When `[checkpoint]` is configured

1. Fetch notes from remote (best-effort).
2. Walk git log backwards from HEAD, looking for the most recent checkpoint commit (a commit whose diff touches any file in `build_inputs`).
3. **If a checkpoint is found:**
   a. Check the note on that commit for a cached checkpoint image (keyed by TOML config path).
   b. If no cached image exists: build the checkpoint image (full build), write the note, push notes.
   c. Rust generates a unified binary patch file locally using a temporary git index: `git read-tree <checkpoint-sha>` into a temp index, then `git add -A` to stage the entire working tree (tracked + untracked), then `git diff --cached --binary <checkpoint-sha>` against the temp index. This produces a single patch that includes both tracked modifications and untracked (non-ignored) files. The real index is never touched.
   d. If diff is empty and no untracked files: use checkpoint image directly (zero overhead).
   e. If non-empty: pass the patch file to the provider via `provider.prepare_from_checkpoint()`, which builds the appropriate command (e.g. `uv run @modal_sandbox.py prepare --from-base-image=... --patch-file=...`) and applies it on top of the checkpoint image. All git logic stays in Rust.
   f. Skip `include_cwd`, `copy_dirs`, `sandbox_init_cmd` (all baked into the checkpoint image).
4. **If no checkpoint is found** in the search window: build a full image (same as non-checkpoint mode).

### When `[checkpoint]` is absent

The latest-commit path follows the same pipeline as the checkpoint path. The only difference is how the base commit is selected (HEAD instead of nearest ancestor touching `build_inputs`).

1. Fetch notes from remote (best-effort).
2. Look up the latest commit (HEAD) in git notes for a cached base image (keyed by TOML config path).
3. If cached base image exists (cache hit): Rust generates a unified binary patch file locally using a temporary git index (same technique as checkpoint mode: temp index seeded with HEAD tree, `git add -A`, `git diff --cached --binary HEAD`), then passes it to the provider's `prepare_from_checkpoint()` method which builds a thin-diff image on top of the cached base. On failure (e.g. git not installed in image, apply failure), warn and fall back to full build.
4. If no cached base image (cache miss): export the HEAD tree via `git::export_tree()`, build the base image via `provider.prepare(context_dir=exported_tree)`, write the note on HEAD, push notes. Then apply thin diff (same as step 3). The thin-diff step routes through `provider.prepare_from_checkpoint()` rather than directly calling a `ShellConnector`.

### When `--no-cache` is passed

`--no-cache` means "do not read or write the git notes cache." It does NOT mean "use a different build procedure." The image build itself must be identical to what a normal run would produce -- only the cache interactions are skipped.

Both Checkpoint and LatestCommit follow the same unified procedure under `--no-cache`:

1. Do not fetch or read git notes (no cache lookup).
2. Resolve the base commit (same logic as normal path):
   - With `[checkpoint]` config: walk git log backwards to find the nearest checkpoint commit.
   - Without `[checkpoint]` config: use HEAD as the base commit.
3. If a base commit is found:
   a. Export the base commit tree via `git::export_tree()`.
   b. Build the base image via `provider.prepare(context_dir=exported_tree)` -- the same build procedure as a normal cache miss.
   c. Apply thin diff on top (same as normal path).
   d. Do not write notes. Do not push notes.
4. If no base commit is found (no checkpoint in window, or empty repo with no commits): full build (same as normal path).

This ensures the resulting image is identical to what the normal path would produce. The build procedure uses `context_dir` (the exported tree) so that `COPY . /app` copies a clean, deterministic checkout of the base commit. Skipping this procedure and falling through to a plain full build would use the live working directory as context, producing a different (and likely broken) image.

### Cache Expiry

Modal (and other providers) may garbage-collect images at any time. A cached image ID in a git note can become stale. Offload must handle this gracefully:

- Attempt to use the cached image.
- If the provider reports the image does not exist: catch the error.
- Warn user visibly (e.g. `[checkpoint] Cached image im-abc123 expired, rebuilding...`).
- Rebuild the image from scratch.
- Update the note with the new image ID.
- Push the updated notes ref to remote.

This applies to both checkpoint images and regular cached images. The system is self-healing: a single expired image causes one slow run, after which the cache is repopulated for all users.

## `offload checkpoint-status` Command

A read-only diagnostic command that shows the current cache state for the working directory. This gives users visibility into the otherwise-opaque caching machinery. It works in both modes: with `[checkpoint]` config (checkpoint caching) and without it (latest-commit caching).

Output includes:
- Current HEAD SHA.
- Base commit SHA with a contextual qualifier indicating the mode.
- Whether a cached image exists for the base commit (and its image ID).
- Whether thin diff mode will be used for the next run.

**With `[checkpoint]` config:** Resolves the nearest checkpoint ancestor (a commit touching `build_inputs`). Shows the checkpoint SHA and distance from HEAD. If no checkpoint is found within the search window, reports "no checkpoint found in last N commits" and next run mode as full build.

**Without `[checkpoint]` config:** Resolves the latest commit (HEAD) as the base. Shows the HEAD SHA. If there is no commit (empty repo), reports "no base commit" and next run mode as full build.

Example output (checkpoint mode, cache hit):
```
HEAD:               a1b2c3d4
Base commit:        f5e6d7c8 (checkpoint, 3 commits back)
Cached image:       im-abc123
Next run mode:      thin diff (2 files changed since checkpoint)
```

Example output (latest-commit mode, cache hit):
```
HEAD:               a1b2c3d4
Base commit:        a1b2c3d4 (latest commit, HEAD)
Cached image:       im-def456
Next run mode:      thin diff (1 files changed since HEAD)
```

Example output (latest-commit mode, no commits):
```
HEAD:               (none -- empty repo)
Next run mode:      full build
```

## Edge Cases

| Case | Behavior |
|------|----------|
| First run, no notes exist | Build image, write note, push |
| Checkpoint found with cached image | Use cached checkpoint + thin diff |
| Checkpoint found, no cached image | Build checkpoint image, cache it, apply thin diff |
| Cached image expired on provider | Warn, rebuild, update note |
| `offload run --no-cache` (with `[checkpoint]`) | Same as normal cache miss (find checkpoint SHA, export tree, build base from `context_dir`, apply thin diff) -- but no note read/write. Produces the same image, just without persisting the result. |
| `offload run --no-cache` (without `[checkpoint]`) | Same as normal cache miss (resolve HEAD SHA, export HEAD tree, build base from `context_dir`, apply thin diff of uncommitted changes) -- but no note read/write. Produces the same image, just without persisting the result. |
| Empty diff since checkpoint | Use checkpoint image directly |
| Binary files in diff | `git diff --binary` + `git apply` handles them |
| New untracked files since checkpoint | Detected by Rust via a temporary git index (`git add -A` stages all untracked non-ignored files, then `git diff --cached --binary` includes them in the unified patch). Included in the binary patch file alongside tracked changes (not baked into the checkpoint image itself) |
| No `[checkpoint]` in config, first run (latest-commit caching) | Export HEAD tree, build base from `context_dir`, cache on HEAD, then thin diff of uncommitted changes |
| No `[checkpoint]` in config, cached (latest-commit caching) | Use cached HEAD image + thin diff of uncommitted changes |
| No `[checkpoint]` in config, initial commit (latest-commit caching) | Full build, no caching |
| Local provider | No notes interaction (local doesn't build images) |
| History rewritten (rebase/amend) | New commit has no note; first run builds and caches |
| Multiple team members | Notes pushed/fetched via remote; all share same mapping |
| Multiple TOML configs in same repo | Notes keyed by config path; no collision |
| Config path changed (rename) | Old key orphaned; new key triggers fresh build |

## Superseded Mechanisms

Git notes replace all prior file-based caching mechanisms. The following files are no longer used by offload and should be removed from repos that adopt this feature:

| File | Former purpose | Replaced by |
|------|---------------|-------------|
| `.offload-image-cache` | Local cache of Dockerfile base image ID (in `modal_sandbox.py`) | Git notes on the commit |
| `.offload-base-commit` | Pinned checkpoint commit SHA (in mngr/sculptor justfiles) | Automatic checkpoint detection via `build_inputs` |
| `.offload-cache-key` | Hash of build inputs for invalidation (in mngr justfile) | Checkpoint detection via `build_inputs` (commit SHA is the cache key) |

The `read_cached_image_id()`, `write_cached_image_id()`, and `clear_image_cache()` functions in `modal_sandbox.py` were removed. The `--cached` flag on the `prepare` command is kept as a hidden, deprecated no-op for backward compatibility (not fully removed). Git notes become the sole caching mechanism.

Modal's own internal Dockerfile layer caching (within their build infrastructure) is unaffected -- that is a provider-side optimization, not something offload manages.

## Backward Compatibility

- Existing `offload.toml` files without `[checkpoint]` continue to work. The new behavior is that the HEAD tree is exported and used to build a base image (cached in git notes on HEAD), and subsequent runs apply a thin diff of uncommitted changes. This follows the same pipeline as checkpoint mode -- the only difference is that the base commit is HEAD instead of the nearest checkpoint ancestor.
- Projects using the old `.offload-base-commit` + `.offload-image-cache` pattern (mngr, sculptor) can migrate by: (1) adding a `[checkpoint]` section, (2) deleting the old cache files, (3) running `offload run` -- the first run builds and caches automatically.
