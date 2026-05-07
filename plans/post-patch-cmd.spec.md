# Post-Patch Command -- Specification

This document specifies the requirements for the `post_patch_cmd` feature in Offload.

## Problem

Offload's image build pipeline splits the environment into two layers:

1. **Base image** — Dockerfile + source checkout + dependency install + `sandbox_init_cmd`. Cached in git notes. Rebuilt only when `build_inputs` change (checkpoint mode) or HEAD changes (latest-commit mode).
2. **Thin-diff** — Binary patch of working-tree changes applied on top of the base image via `offload apply-diff`. Fast (~4s). Rebuilds every run.

This split assumes a clean dichotomy: things either change-with-deps (heavy, cache-worthy) or change-with-source (cheap, patch-worthy). But there is a third category: **derived artifacts** — files produced by running tools against the source tree. Examples:

- Generated API clients (e.g., from an OpenAPI schema)
- Frontend bundles (e.g., vite build output)
- Generated TypeScript types from a backend schema

These artifacts are:

- **Cheap to produce** (~10–30s each) — don't need heavy-layer caching
- **Derived from source** — should be regenerated when source changes
- **Not in git** — `git apply` cannot update them

Today, derived artifact generation lives in `sandbox_init_cmd`. When a commit changes source files that affect derived artifacts without touching `build_inputs` (e.g., adding an API endpoint without modifying `pyproject.toml`), the checkpoint cache hits, the thin-diff patches the source, but the cached generated client is stale. The result is broken imports or test failures.

## Goals

1. Provide a hook that runs after the thin-diff patch is applied, before the image is materialized.
2. Enable regeneration of derived artifacts on every image build, regardless of whether the build was a cache hit or full build.
3. Keep the execution cost low: run once as an image layer, shared across all sandboxes.

## Non-Goals

- Replacing `sandbox_init_cmd` — that field continues to handle heavy setup (dependency installs, system packages).
- Automatic detection of which artifacts need regeneration — the user provides the command.
- Running the command at sandbox creation time (per-sandbox) — it runs at image build time (once).

## Definitions

**`post_patch_cmd`**: A shell command that runs as an image layer after the thin-diff patch is applied (or after the base image is loaded when there is no diff), before the image is materialized. Complementary to `sandbox_init_cmd`: init handles heavy setup, post-patch handles derived artifacts.

**`OFFLOAD_PATCH_FILE`**: An environment variable set to the path of the binary patch file inside the image (e.g., `/tmp/offload.patch`) when a diff exists. Unset when there is no diff. Allows the command to inspect what changed and conditionally regenerate only affected artifacts.

## Configuration

A new optional field in the `[offload]` section of `offload.toml`:

```toml
[offload]
sandbox_init_cmd = "uv sync --all-packages"
post_patch_cmd = "scripts/regen-clients.sh"
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `post_patch_cmd` | `string` | No | Shell command to run after thin-diff patch is applied, before image materialization. |

### Semantics

- If absent or `None`, no post-patch command runs. Existing behavior is unchanged.
- If present, the command runs as a `run_commands()` image layer in `_derive_image_from_base()`.
- The command runs in **all** image build scenarios — cache hit, cache miss, with diff, without diff.
- With local provider: silently ignored (no image build step). Same policy as `sandbox_init_cmd`.
- No provider-type restriction at validation time. Same policy as `sandbox_init_cmd`.

## Execution Model

`post_patch_cmd` runs in ONE place: `_derive_image_from_base()` in `modal_sandbox.py`. The four scenarios:

| Scenario | `sandbox_init_cmd` | Patch applied? | `post_patch_cmd` |
|----------|-------------------|----------------|-----------------|
| Full build + diff | Runs in `_build_final_image` | Yes | Runs in `_derive_image_from_base` |
| Full build + no diff | Runs in `_build_final_image` | No | Runs in `_derive_image_from_base` |
| Cache hit + diff | Already baked in base | Yes | Runs in `_derive_image_from_base` |
| Cache hit + no diff | Already baked in base | No | Runs in `_derive_image_from_base` |

### Sequence within `_derive_image_from_base`

When a diff exists:

1. Add patch file to image at `/tmp/offload.patch`
2. Apply patch: `offload apply-diff /tmp/offload.patch --project-root {project_root}`
3. Run `post_patch_cmd` with `OFFLOAD_PATCH_FILE=/tmp/offload.patch`
4. Clean up: `rm /tmp/offload.patch`
5. Materialize image

When no diff exists:

1. Run `post_patch_cmd` (without `OFFLOAD_PATCH_FILE` in environment)
2. Materialize image

### Behavior change to `try_thin_diff`

Currently, `try_thin_diff` short-circuits when there is no diff:

```rust
if patch is None {
    return Ok(base_image_id.to_string());
}
```

When `post_patch_cmd` is configured, `try_thin_diff` must still call `build_incremental` even when there is no diff, passing `None` for the patch file. This ensures `post_patch_cmd` always runs.

When `post_patch_cmd` is `None` and there is no diff, the existing short-circuit behavior is preserved.

## CLI Interface (modal_sandbox.py)

New option on the `prepare` command:

```
--post-patch-cmd TEXT    Command to run after thin-diff patch is applied
```

Passed from Rust to `modal_sandbox.py` as `--post-patch-cmd=<shell-quoted value>` in the `build_incremental` prepare command. Omitted when `None`.

When `--from-base-image` is provided without `--patch-file` but with `--post-patch-cmd`, `_derive_image_from_base` skips patch application and runs only the post-patch command.

## Implementation Surface

### Config schema (`src/config/schema.rs`)
- Add `pub post_patch_cmd: Option<String>` to `OffloadConfig`, with `#[serde(default)]`, adjacent to `sandbox_init_cmd`.

### Provider trait (`src/provider.rs`)
- Add `pub post_patch_cmd: Option<&'a str>` to `PrepareContext`.

### Main entry (`src/main.rs`)
- Thread `config.offload.post_patch_cmd.as_deref()` into `PrepareContext`.

### ImageBuilder trait (`src/image_cache.rs`)
- Add `post_patch_cmd: Option<&str>` parameter to `build_incremental`.
- In `try_thin_diff`: when `post_patch_cmd` is `Some` and there is no diff, still call `build_incremental` with `None` for patch file.
- Pass `post_patch_cmd` through at all `build_incremental` call sites.

### Modal provider (`src/provider/modal.rs`)
- Accept `post_patch_cmd: Option<&str>` in `build_incremental`.
- Append `--post-patch-cmd=<quoted>` to the prepare command when present.
- When no patch file but `post_patch_cmd` is set, still invoke `modal_sandbox.py prepare` with `--from-base-image` and `--post-patch-cmd` (no `--patch-file`).

### Default provider (`src/provider/default.rs`)
- Same changes as modal provider.

### modal_sandbox.py (`scripts/modal_sandbox.py`)
- Add `--post-patch-cmd` CLI option to `prepare` command.
- Pass through to `_derive_image_from_base`.
- Implement the three-layer sequence: apply patch → run post_patch_cmd → clean up.

## Edge Cases

| Case | Behavior |
|------|----------|
| `post_patch_cmd` not configured | No behavior change. Existing `try_thin_diff` short-circuit preserved. |
| `post_patch_cmd` configured, no diff | `build_incremental` called with no patch. `post_patch_cmd` runs without `OFFLOAD_PATCH_FILE`. |
| `post_patch_cmd` configured, with diff | Patch applied, then `post_patch_cmd` runs with `OFFLOAD_PATCH_FILE=/tmp/offload.patch`. Patch cleaned up after. |
| `post_patch_cmd` fails (non-zero exit) | Image build fails. Same error handling as `sandbox_init_cmd` failure. |
| Local provider | `post_patch_cmd` silently ignored. |
| `post_patch_cmd` set without `[checkpoint]` | Valid. Works with latest-commit caching. `post_patch_cmd` runs in the thin-diff step. |
| `post_patch_cmd` and `sandbox_init_cmd` both set | Both run. `sandbox_init_cmd` in `_build_final_image` (base build), `post_patch_cmd` in `_derive_image_from_base` (thin-diff step). |
| `--no-cache` with `post_patch_cmd` | Full build proceeds as normal, then thin-diff step runs `post_patch_cmd`. No caching interaction. |

## Testing

- Config round-trip serialization test for `post_patch_cmd` (following `test_sandbox_init_cmd_round_trip` pattern).
- Provider unit tests verifying `build_incremental` prepare command includes `--post-patch-cmd` with proper shell quoting.
- Mock `ImageBuilder` test verifying `try_thin_diff` calls `build_incremental` when `post_patch_cmd` is set and diff is empty.
- End-to-end validation in downstream repos (sculptor, mng) by adding `post_patch_cmd` to their `offload.toml`.

## Backward Compatibility

- Existing `offload.toml` files without `post_patch_cmd` are unaffected. The field defaults to `None`.
- The `build_incremental` trait signature changes (new parameter). This is an internal API — no external consumers.
- No breaking changes to CLI interface. The `--post-patch-cmd` option is additive.
