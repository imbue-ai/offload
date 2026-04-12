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

## Definitions

**Checkpoint**: A commit whose diff (relative to any of its parents) modifies any file in the `build_inputs` set. For merge commits (including octopus merges), the diff is checked against all parents using `git diff-tree --no-commit-id --name-only -r -m <sha>`. Whether a commit is a checkpoint is a pure function of the commit's content and the config -- it is not a manual designation.

**Checkpoint image**: A full image built from a checkpoint commit (Dockerfile base + source checkout + dependency install + sandbox_init_cmd). Expensive to build. Cached in git notes.

**Cached offload image**: When there is no `[checkpoint]` section in the config, offload builds and caches a regular (non-checkpoint) image for each commit. Also stored in git notes.

**Build inputs hash**: A content fingerprint of the files listed in `build_inputs`. Used to determine whether a cached checkpoint image is still valid for the current state of those files.

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
Cached offload image (one per commit):
  Dockerfile base + source + sandbox_init_cmd
  Cached in git notes on the commit itself.
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

- If the `[checkpoint]` section is absent, checkpoints do not exist. Offload builds and caches a regular image per commit.
- If the section is present, `build_inputs` must be non-empty.
- The listed files are the "build spec" -- Dockerfiles, dependency manifests, build/install scripts.
- A commit is a checkpoint if and only if its diff (vs any parent) touches any file in this list. For merge commits, all parents are checked.
- Offload computes a build inputs hash of these files and stores it alongside the image ID in the note, so it can verify the cached checkpoint is still valid.

## Metadata Storage: Git Notes

### Design

- Notes ref: `refs/notes/offload-images`
- Key: Git commit SHA
- Value: JSON object keyed by TOML config file path (repo-relative, no `./` prefix, e.g. `offload.toml` not `./offload.toml`)

Example note on a commit (two configs in the same repo):

```json
{
  "offload-modal.toml": {
    "image_id": "im-abc123",
    "build_inputs_hash": "e5f6a7b8"
  },
  "offload-integration.toml": {
    "image_id": "im-def456",
    "build_inputs_hash": "c3d4e5f6"
  }
}
```

When `[checkpoint]` is absent from the config, the entry looks like:

```json
{
  "offload.toml": {
    "image_id": "im-789ghi"
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

Multiple `offload run` invocations (e.g. parallel CI jobs) may attempt to write notes to the same commit simultaneously. The concurrency policy is **last write wins** with a fetch-before-push strategy to minimize data loss:

1. Before writing a note, fetch the latest notes ref from the remote.
2. Read-modify-write the note JSON locally (merge entries so two configs don't clobber each other).
3. Force-push the notes ref.

This shrinks the race window to the time between the fetch and the push, rather than the entire duration of the image build. In the worst case, a concurrent write still overwrites another's entry, and the next run that needs the lost entry rebuilds and re-caches it. This is acceptable because image builds are idempotent and the cost of a redundant rebuild is low relative to the complexity of a locking protocol.

**WARNING**: Concurrent runs against the same commit may lose each other's cached images. For expensive checkpoint images, avoid parallel CI jobs targeting the same commit.

### jj Support

- Offload operates on the underlying colocated Git repo (`.git/` directory).
- The mapping is keyed by Git commit SHA, not jj change ID.
- Metadata is NOT automatically copied across rebases or amends. If history is rewritten, the new commit has no note; the first `offload run` against it will build and cache a new image.

### Build Inputs Hash

- Computed from the concatenated contents of all files listed in `build_inputs`, sorted lexicographically by path.
- All files listed in `build_inputs` must exist. If any file is missing, offload reports an error and refuses to proceed. This prevents silent hash changes from deleted files.
- Used to verify that a cached checkpoint image is still valid (the build input files haven't been modified outside of a checkpoint commit, e.g. via a merge or manual edit).
- Stored in the note alongside the image ID.

## `offload run` Flow

### When `[checkpoint]` is configured

1. Fetch notes from remote (best-effort).
2. Walk git log backwards from HEAD, looking for the most recent checkpoint commit (a commit whose diff touches any file in `build_inputs`).
3. **If a checkpoint is found:**
   a. Check the note on that commit for a cached checkpoint image (keyed by TOML config path).
   b. If no cached image exists: build the checkpoint image (full build), write the note, push notes.
   c. Verify the build inputs hash matches the current content of the `build_inputs` files.
      - If mismatch: warn with an actionable message listing the `build_inputs` files and suggesting the user either commit their changes (to create a new checkpoint) or revert them (to use the cached image). Example: `[checkpoint] Build inputs hash mismatch. Files in build_inputs may have been modified without a checkpoint commit. Check: Dockerfile, requirements.txt. Commit changes to create a new checkpoint, or revert to use the cached image.` Fall back to full build for this run.
   d. Generate `git diff <checkpoint-sha> HEAD --binary`.
   e. If diff is empty: use checkpoint image directly (zero overhead).
   f. If diff is non-empty: build thin current image by applying diff on top of checkpoint image. Untracked files (detected via `git ls-files --others --exclude-standard`) are included in the source tarball alongside the diff, not baked into the checkpoint image.
   g. Skip `include_cwd`, `copy_dirs`, `sandbox_init_cmd` (all baked into the checkpoint image).
4. **If no checkpoint is found** in the search window: build a full image (same as non-checkpoint mode).

### When `[checkpoint]` is absent

1. Fetch notes from remote (best-effort).
2. Check the note on HEAD for a cached image (keyed by TOML config path).
3. If cached image exists: use it.
4. If no cached image: build a full image, write the note on HEAD, push notes.

### When `--no-cache` is passed

Ignore all cached images (both checkpoint and regular). Full build. Do not read or write notes.

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

A read-only diagnostic command that shows the current checkpoint state for the working directory. This gives users visibility into the otherwise-opaque checkpoint machinery.

Output includes:
- Current HEAD SHA.
- Nearest checkpoint commit (if any) and its SHA, or "no checkpoint found in last N commits".
- Whether a cached image exists for that checkpoint (and its image ID).
- Current build inputs hash vs cached hash (match/mismatch).
- Whether thin diff mode will be used for the next run.

This command requires a `[checkpoint]` section in the config. If absent, it reports that checkpoint mode is not configured.

Example output:
```
HEAD:               a1b2c3d4
Checkpoint:         f5e6d7c8 (3 commits back)
Cached image:       im-abc123
Build inputs hash:  e5f6a7b8 (matches cached)
Next run mode:      thin diff (2 files changed since checkpoint)
```

## Edge Cases

| Case | Behavior |
|------|----------|
| First run, no notes exist | Build image, write note, push |
| Checkpoint found with cached image | Use cached checkpoint + thin diff |
| Checkpoint found, no cached image | Build checkpoint image, cache it, apply thin diff |
| Cached image expired on provider | Warn, rebuild, update note |
| Build inputs hash mismatch | Warn with actionable message (list affected files, suggest commit or revert), fall back to full build |
| `offload run --no-cache` | Full build, no note read/write |
| Empty diff since checkpoint | Use checkpoint image directly |
| Binary files in diff | `git diff --binary` + `git apply` handles them |
| New untracked files since checkpoint | Detected via `git ls-files --others --exclude-standard`, included in the source tarball sent to the image (not baked into the checkpoint image itself) |
| No `[checkpoint]` in config, first run | Build image, cache in note on HEAD |
| No `[checkpoint]` in config, cached | Use cached image from note on HEAD |
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
| `.offload-cache-key` | Hash of build inputs for invalidation (in mngr justfile) | Build inputs hash stored in git note |

The `read_cached_image_id()`, `write_cached_image_id()`, and `clear_image_cache()` functions in `modal_sandbox.py` will be removed. The `--cached` flag on the `prepare` command will be removed. Git notes become the sole caching mechanism.

Modal's own internal Dockerfile layer caching (within their build infrastructure) is unaffected -- that is a provider-side optimization, not something offload manages.

## Backward Compatibility

- Existing `offload.toml` files without `[checkpoint]` continue to work. The only new behavior is that image IDs are cached in git notes (today images are rebuilt every run).
- Projects using the old `.offload-base-commit` + `.offload-image-cache` pattern (mngr, sculptor) can migrate by: (1) adding a `[checkpoint]` section, (2) deleting the old cache files, (3) running `offload run` -- the first run builds and caches automatically.
