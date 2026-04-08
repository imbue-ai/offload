# Checkpoint Images -- Specification

This document specifies the requirements for checkpoint image support in Offload.
It supersedes the earlier draft in `#checkpoint-images.md#`.

## Problem

Modal image builds are slow. Every `offload run` rebuilds: base image + source overlay + dependency install. The sculptor and mngr repos work around this with manual "keyframe" systems using committed files (`.offload-base-commit`, `.offload-image-cache`). This is clunky: the files require their own commits and are easy to forget to update.

This feature builds the checkpoint pattern into Offload as a first-class capability, using git notes instead of committed files for metadata storage.

## Goals

1. Make image selection/building for sandboxed test runs Just Work for users.
2. Avoid rebuilding expensive dependency/download layers on every commit.
3. Keep a single source of truth for commit → Modal image mapping.
4. Store metadata without mutating commit SHAs.
5. Support both Git and jj users with the same backend mechanism.
6. Keep the system easy to bootstrap via `offload init`.

## Non-Goals

- Automatically creating checkpoint images (user must explicitly run `offload checkpoint`).
- Automatically copying metadata across rebases/amends (new commits get their own mapping).
- Replacing Modal's internal Dockerfile layer caching (that remains as-is).

## Two-Image Model

```
Checkpoint image (rebuilt infrequently):
  Dockerfile base + source checkout + dependency install + sandbox_init_cmd

Current image (rebuilt each run):
  Checkpoint image + thin git diff of changes since checkpoint
```

When no checkpoint exists, existing single-image behavior is preserved unchanged.

## Configuration: `[checkpoint]` Section

A new optional TOML section in `offload.toml`:

```toml
[checkpoint]
image_identity = [
    "Dockerfile",
    "requirements.txt",
    "setup.py",
    "pyproject.toml",
]
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `image_identity` | `list[str]` | Yes (if section present) | Repo-relative file paths. A change to ANY of these files means the checkpoint image must be rebuilt. |

### Semantics

- If the `[checkpoint]` section is absent, checkpointing is disabled. All behavior is unchanged from today.
- If the section is present, `image_identity` must be non-empty.
- The listed files are the "build spec" -- Dockerfiles, dependency manifests, build/install scripts.
- Offload computes a content fingerprint of these files (the "identity hash") and stores it alongside the image ID in the checkpoint note.
- On `offload run`, offload compares the current identity hash against the stored one. If they differ, the checkpoint is stale and a full build occurs (with a warning to re-run `offload checkpoint`).

## Metadata Storage: Git Notes

### Design

- Notes ref: `refs/notes/offload-images`
- Key: Git commit SHA
- Value: JSON string, e.g. `{"image_id":"im-32147gl084235794327","identity_hash":"a1b2c3d4e5f6"}`

### Properties

- Attaches metadata to a commit without changing the commit SHA.
- The notes ref is stored on GitHub (or other remote) but is not visible in normal GitHub UI.
- Users interact with it exclusively through `offload` commands, never directly.
- `offload` explicitly reads, writes, fetches, and pushes notes.
- `offload init` (or first `offload checkpoint`) configures the local repo to auto-fetch the notes ref.

### jj Support

- Offload operates on the underlying colocated Git repo (`.git/` directory).
- The mapping is keyed by Git commit SHA, not jj change ID.
- Metadata is NOT automatically copied across rebases or amends. If history is rewritten, the new commit has no note and must be re-checkpointed.

### Identity Hash

- Computed from the concatenated contents of all files listed in `image_identity`, sorted lexicographically by path.
- Used as a fingerprint to detect when the build spec has changed since the last checkpoint.
- Stored in the note alongside the image ID.

## CLI: `offload checkpoint`

```
offload checkpoint              # Build checkpoint image, write note, push
offload checkpoint --delete     # Remove checkpoint note from HEAD
offload checkpoint --no-cache   # Force fresh base image rebuild
offload checkpoint --remote X   # Use remote X instead of origin (default: origin)
```

### Preconditions

- `[checkpoint]` section must be present in config.
- Working tree must be clean (`git status --porcelain` is empty).
  - In jj, this means `@` must be empty (changes are in `@-` or earlier).
- Provider must not be `local`.

### Behavior

1. Verify clean working tree.
2. Fetch latest notes from remote (best-effort; may fail if no notes exist yet).
3. Get HEAD commit SHA via `git rev-parse HEAD`.
4. Compute identity hash from `image_identity` files.
5. Build checkpoint image via provider (full build: Dockerfile + source + copy_dirs + sandbox_init_cmd).
6. Write git note on HEAD: `{"image_id":"<id>","identity_hash":"<hash>"}`.
7. Configure notes fetch refspec (idempotent).
8. Push notes ref to remote.

### `--delete` Behavior

Remove the note from HEAD (if one exists) and exit.

## Modified `offload run` Flow

### When `[checkpoint]` is configured

1. Fetch notes from remote (best-effort).
2. Walk git log backwards from HEAD, looking for the most recent commit with an `offload-images` note (bounded search, e.g. 100 commits).
3. If a note is found:
   a. Compute the current identity hash from `image_identity` files.
   b. If identity hash matches the note: use checkpoint.
      - Generate `git diff <checkpoint-sha> HEAD --binary`.
      - If diff is empty: use checkpoint image directly (zero overhead).
      - If diff is non-empty: build thin "current image" by applying diff on top of checkpoint image.
      - Skip `include_cwd`, `copy_dirs`, `sandbox_init_cmd` (all baked into checkpoint).
   c. If identity hash does NOT match: warn user ("identity files changed, run `offload checkpoint`"), fall back to full build.
4. If no note is found: inform user, fall back to full build.

### When `[checkpoint]` is absent

Existing behavior, completely unchanged.

### When `--no-cache` is passed

Ignore checkpoint, full build (existing behavior).

### Cache Expiry

If the checkpoint image has expired on Modal (or other provider):
- Catch the error.
- Warn user visibly.
- Fall back to full build.
- Do NOT automatically update the note (user should re-run `offload checkpoint`).

## Edge Cases

| Case | Behavior |
|------|----------|
| First run, no checkpoint | Unchanged from today |
| After `offload checkpoint` | `offload run` uses thin diff layer |
| Checkpoint image expired on provider | Warn, fall back to full build |
| Files in `image_identity` changed since checkpoint | Warn, fall back to full build |
| `offload run --no-cache` | Ignore checkpoint, full build |
| Empty diff (no changes since checkpoint) | Use checkpoint image directly |
| Binary files in diff | `git diff --binary` + `git apply` handles them |
| New untracked files since checkpoint | Detected via `git ls-files --others --exclude-standard`, included alongside diff |
| `offload checkpoint --delete` | Remove note from HEAD |
| No `[checkpoint]` in config | Feature entirely disabled |
| Local provider with `[checkpoint]` | Validation error |
| History rewritten (rebase/amend) | New commits have no note; user must re-checkpoint |
| Multiple team members | Notes pushed/fetched via remote; all share same mapping |

## Backward Compatibility

- Existing `offload.toml` files without `[checkpoint]` continue to work with zero changes.
- The `.offload-image-cache` file used internally by `modal_sandbox.py` for Dockerfile base image caching is unaffected.
- Projects using the old manual `.offload-base-commit` + `.offload-image-cache` pattern (mngr, sculptor) can migrate by adding a `[checkpoint]` section and running `offload checkpoint`.
