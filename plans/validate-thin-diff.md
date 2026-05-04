# Validation Plan

## Prerequisites

Both repos use checkpoint mode with thin diffs, so they exercise the exact code path we changed. The key difference: sculptor has a complex `sandbox_init_cmd` that uses git; mngr does not.

Install the branch build of offload so both repos pick it up:

```bash
cargo install --path .    # from offload repo
```

## Phase 1: mngr (simpler — no sandbox_init_cmd)

mngr has 3 offload configs, all with `[checkpoint]`. No `sandbox_init_cmd`, so the sandbox filesystem matches the checkpoint tree exactly — thin diffs should apply cleanly.

1. `just test-offload` (offload-modal.toml) — the primary config. Run it, confirm:
   - Thin diff applies (look for `[prepare] Building thin diff image...` in output, not a full rebuild)
   - Tests pass at the same rate as before (compare pass/fail counts)
2. Force a new-file scenario: create a throwaway untracked file (e.g. `touch libs/mngr/test_sentinel.py`), run `just test-offload` again. This is the specific failure case `code-5nl` fixes — confirm the thin diff includes the new file and applies without error.
3. Acceptance and release configs (`just test-offload-acceptance`, `just test-offload-release`) — run once each, confirm thin diff applies and tests pass.

## Phase 2: sculptor (harder — git-heavy sandbox_init_cmd)

Sculptor's `sandbox_init_cmd` runs `git init`, `git config`, and other git operations inside the sandbox. The checkpoint image has post-`sandbox_init_cmd` state, so the thin diff applies on top of a filesystem that has a `.git/` directory and potentially modified files.

1. `just test-offload` — run it, confirm:
   - Thin diff applies cleanly
   - No regression in test pass/fail counts
2. New-file scenario: `touch sculptor/test_sentinel.py`, run again. Confirm thin diff includes it.
3. Watch for: `sandbox_init_cmd` modifies tracked files (e.g. npm build outputs in `sculptor/frontend`). If the thin diff's context lines don't match the post-init state, `diffy::apply` will return an error and offload will fall back to full build. This is the expected behavior per the spec (non-goal: fuzzy matching). Log the outcome — if it falls back, note which file's context didn't match for future investigation.

## Phase 3: Dockerfile audit (optional, after Phase 1-2 pass)

Check if `git` in each Dockerfile is only there for thin diffs or also needed at runtime:

- **sculptor**: `git` is needed — `sandbox_init_cmd` runs `git init`, `git config`, `git commit`. Keep it.
- **mngr**: Check if any tests or deps use `git`. If not, try removing `git` from the Dockerfile and re-running. This validates the "drop git" upgrade path.

## Success criteria

| Check | Expected |
|---|---|
| mngr thin diff applies (normal) | `[prepare] Building thin diff image...` in output |
| mngr thin diff applies (new file) | Same, no "does not exist in index" error |
| sculptor thin diff applies (normal) | Same |
| sculptor thin diff applies (new file) | Same |
| Test pass/fail counts unchanged | Compare against a baseline run on the previous offload version |

## Rollback

If either repo fails: `cargo install offload` (reinstalls the published version from crates.io, reverting to `git apply --3way`).
