# Thin-Diff Patch Application via `diffy` -- Specification

This document specifies the replacement of `git apply --3way` with `offload apply-diff`, a new subcommand that uses the `diffy` crate to apply patches inside the sandbox.

## Problem

The thin-diff step generates a binary patch (`git diff --cached --binary`) from the checkpoint commit to the current working tree, then ships it to the Modal sandbox for application via `git apply --3way`. This fails in two known scenarios:

1. **New files**: `--3way` requires blob objects in the sandbox's git object database. The sandbox has a fresh `git init` with no history — blobs referenced in the patch's `index` lines don't exist. New files (e.g., `offload-history.jsonl`) cause `--3way` to error with "does not exist in index", triggering a costly cold-build fallback.

2. **`sandbox_init_cmd`-modified files**: The base image is built from the checkpoint tree, then `sandbox_init_cmd` runs (e.g., `uv sync`), potentially modifying tracked files. The patch's pre-image assumes checkpoint file state, but the sandbox has post-`sandbox_init_cmd` state. Context lines don't match, and `--3way` can't help because the checkpoint blobs aren't in the sandbox's object database.

Both scenarios cause silent fallback to full image builds, adding minutes to CI runs.

### History

The original checkpoint-images plan specified `git apply` without `--3way`. Subsequent commits added `--3way` (e247ead), then `git add -A && git commit` to support it (2cd69b9). The `--3way` flag was intended to handle content mismatches but is fundamentally broken in this architecture: the sandbox's fresh git repo never has the checkpoint's blob objects, so `--3way`'s merge-base lookup always fails when content differs.

## Goals

1. Eliminate thin-diff failures caused by new files in the patch.
2. Replace `git apply` with a purpose-built subcommand (`offload apply-diff`) that uses `diffy` for patch application.
3. Handle text patches, binary patches (literal and delta), new files, and deleted files.
4. Preserve the cached-image reuse architecture: the derived image is built by layering changes onto the cached base image, not by rebuilding from scratch.

## Non-Goals

- Fuzzy matching (tolerating context-line mismatches via Levenshtein similarity). If a text hunk's context doesn't match, the thin diff fails and falls back to cold build. This is acceptable — `sandbox_init_cmd` rarely modifies the same lines that a developer changes.
- Replacing `generate_checkpoint_diff` (the diff generation step stays as-is).
- Changing the base image build pipeline.

## Design

### Dependency: `diffy` 0.5.0

Add `diffy = { version = "0.5", features = ["binary"] }` to `Cargo.toml`.

`diffy` 0.5.0 provides:
- `PatchSet`: streaming parser for multi-file git-format diffs, discriminating `PatchKind::Text` vs `PatchKind::Binary` per file.
- `BinaryPatch`: application of git binary patches (literal and delta format, base85 + zlib).
- Text patch application via `apply()` / `apply_bytes()`.
- Per-file iteration: parse returns individual `FilePatch` entries that can be applied independently.

License: MIT OR Apache-2.0. Compatible with Offload's MIT license.

### New subcommand: `offload apply-diff`

```
offload apply-diff <patch-file> [--project-root <path>]
```

Applies a git-format binary patch to the filesystem at `project-root` (default: current directory). Intended to run inside the sandbox image during the thin-diff step.

Implementation:

1. Read the patch file as bytes.
2. Parse with `diffy::PatchSet` to iterate per-file entries.
3. For each `FilePatch`:
   - **Create (text)**: apply hunks to an empty base; write result to disk.
   - **Create (binary)**: `BinaryPatch` literal format is self-contained; write content to disk.
   - **Delete**: remove the file.
   - **Modify (text)**: read the existing file from disk, apply hunks via `diffy::apply()`, write back.
   - **Modify (binary)**: read the existing file, apply via `BinaryPatch::apply()`, write back.
   - **Rename/copy**: handle accordingly.
4. Exit 0 on success, non-zero on any failure.

### Architecture

The existing architecture is preserved. The only change is the application mechanism inside the sandbox:

```
[Rust] generate_checkpoint_diff → git patch file
[Rust] ship patch file to Python via build_incremental()
[Python] _derive_image_from_base():
           add patch file to image
           run: offload apply-diff /tmp/offload.patch --project-root {project_root}
           snapshot derived image
```

The sandbox image must have `offload` installed (same version as the orchestrator). This is already a natural requirement — customers install offload to run tests.

### Changes to `_derive_image_from_base` in `modal_sandbox.py`

From:
```python
f"cd {project_root} && git init -q . && git config user.email offload@offload && git config user.name offload && git add -A && git commit -q --allow-empty -m base && git apply --3way /tmp/offload.patch --allow-empty && rm /tmp/offload.patch"
```

To:
```python
f"offload apply-diff /tmp/offload.patch --project-root {project_root} && rm /tmp/offload.patch"
```

No `git init`, no `git add`, no `git commit`, no `--3way`. No `git` required in the sandbox image for thin diffs.

### Changes to `ImageBuilder` trait and `try_thin_diff`

No changes. The interface stays the same — `build_incremental` still receives a patch file path. Only the sandbox-side application mechanism changes.

## Edge Cases

| Case | Behavior |
|------|----------|
| New text file | `diffy` applies hunks to empty base; file created |
| New binary file | `BinaryPatch` literal is self-contained; file created |
| Deleted file | Removed from disk |
| Modified text file (sandbox matches checkpoint) | Context matches; hunks apply cleanly |
| Modified text file (sandbox modified by `sandbox_init_cmd`) | Context may not match; `diffy::apply()` returns error; thin diff fails; cold-build fallback |
| Modified binary file | `BinaryPatch::apply()` against existing file |
| Empty patch | `generate_checkpoint_diff` returns `None`; base image used directly (existing behavior) |
| `offload` not installed in sandbox image | `offload apply-diff` command not found; thin diff fails; cold-build fallback with warning |

## Trade-offs

**Gained**:
- New files always work (no blob-lookup dependency).
- Patch application is in Rust, testable via unit tests.
- No `git` required in the sandbox image for thin diffs.
- Cached base image reuse is preserved — no full rebuild penalty.
- Diff artifact stays small (patch, not full file contents).
- `ImageBuilder` trait and `try_thin_diff` are unchanged.

**Cost**:
- New dependency: `diffy 0.5.0` with `binary` feature (pulls in `zlib-rs`).
- `diffy` 0.5.0 is recent (April 2026). Pin to exact version and monitor.
- Sandbox image must have `offload` installed. Customers who use offload for test execution already have this; others may need to add it to their Dockerfile.

## Future Work

- **Fuzzy matching**: If context-line mismatches become a problem (e.g., `sandbox_init_cmd` modifying the same lines a developer changes), evaluate `flickzeug` or contribute fuzzy matching upstream to `diffy`.
