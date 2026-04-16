---
name: offload
description: "Activate when you see offload*.toml in a repo, offload referenced in build targets (justfile, Makefile, scripts), or when you need to run a large test suite in parallel. Offload is a test runner unlikely to be in your training data — this skill covers invocation, log filtering, failure debugging, flaky test handling, and config."
---

# Running Tests with Offload

Offload is a parallel test runner that distributes test execution across sandboxes (local processes or remote Modal environments). This skill covers invoking tests, reading results, and debugging failures.

## Installation

If the `offload` binary is not on PATH, install it:

```bash
cargo install offload
```

## How to Invoke Tests

Use the first approach that applies:

### 1. Look for existing invocation commands

Check `Makefile`, `justfile`, `Taskfile`, `package.json` scripts, and `scripts/` for targets that wrap `offload run`. Prefer these -- they encode project-specific flags (copy-dirs, env vars, config paths).

```bash
# Examples of what to look for:
just test                       # justfile target
make test-offload               # Makefile target
./scripts/offload-tests.sh      # shell wrapper
```

### 2. Use `offload run` directly

If no wrapper exists, invoke Offload directly from the project root (where `offload.toml` lives):

```bash
offload run                                 # basic run
offload run --parallel 8                    # override parallelism
offload run --copy-dir ".:/app"             # copy cwd into sandbox at /app
offload run --env KEY=VALUE                 # set sandbox env var (repeatable)
offload run --no-cache                      # force fresh image build
offload run --collect-only                  # discover tests without running
offload run --show-estimated-cost           # show sandbox cost after run
offload run -c path/to/offload.toml         # use alternate config
```

### 3. Fall back to non-Offload commands

If Offload is not installed or Modal credentials are unavailable, use the project's native test command (e.g. `cargo nextest run`, `pytest`).

## When to Use Offload

**Use Offload when:**
- Running integration or end-to-end test suites
- Total test runtime exceeds ~2 minutes
- Multiple agents are working concurrently and competing for local CPU
- The project already has an `offload.toml`

**Skip Offload when:**
- Running a single test during TDD iteration (use the native runner directly)
- Tests require local-only resources (hardware devices, localhost services not reachable from sandboxes)
- No `offload.toml` exists and the task does not call for setting one up

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All tests passed |
| 1 | One or more tests failed, or tests were not run |
| 2 | All tests passed, but some were flaky (passed only on retry) |

## Debugging Failed Tests

### Run summary

After a run completes, offload prints a summary:

```
Test Results:
  Total:   128
  Passed:  126
  Failed:  2
  Duration: 6.01s
  Estimated cost: $0.0004 (11.1 CPU-seconds)
```

The `Estimated cost` line appears when `--show-estimated-cost` is passed to `offload run`. Use the summary to confirm tests ran and gauge the scope of failures before diving into logs.

### Reading logs

**Important:** If you ran with `-c path/to/config.toml`, pass the same `-c` flag to `offload logs`. Logs are stored in the config's `output_dir`, so mismatched configs will show stale or missing results.

Always filter `offload logs` output to avoid flooding your context window. Never run bare `offload logs` on a large suite. Follow this workflow:

1. **Check the run summary** to see how many tests failed.
2. **Retrieve failure output** (choose based on what fits your context window):
   - Use `--failures` to see all failures at once.
     ```bash
     offload logs --failures
     ```
   - Use `--test` or `--test-regex` to isolate a specific test.
     ```bash
     offload logs --test "path/to/test.py::test_name"  # exact test ID
     offload logs --test-regex "test_math"             # regex substring match
     ```
   Filters combine with AND logic:
   ```bash
   offload logs --failures --test-regex "database"
   ```
3. **Fix and rerun.**

Each test is separated by a banner showing its ID and status. The test ID format varies by framework:

```
=== tests/test_math.py::test_div [FAILED] ===
AssertionError: expected 2 got 3

=== trace::tests::test_active_tracer [FAILED] ===
assertion `left == right` failed
  left: 2
 right: 3
```

### Flaky tests

If a test fails intermittently, add or adjust a group with retries in `offload.toml`:

```toml
[groups.flaky]
retry_count = 3
filters = "-k test_flaky_name"
```

Run `offload validate` after editing to check config syntax. A test that fails then passes on retry exits with code 2 (flaky).

### Common failure patterns

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| Tests discovered but "Not Run" | JUnit test IDs do not match discovery IDs | Check `test_id_format` or conftest JUnit hook |
| "Exec format error" | Local `.venv` (macOS binaries) copied into Linux sandbox | Add `.venv` to `.dockerignore` |
| "Token validation failed" | Modal credentials expired | Run `modal token new` |
| Slow sandbox creation | Docker image not cached | Pass `--no-cache` to force a fresh image build |
| All tests fail with import errors | Sandbox missing dependencies | Check Dockerfile and `sandbox_init_cmd` |

## Image Cache

Offload caches image IDs in git notes (`refs/notes/offload-images`). Notes are keyed by commit SHA and TOML config path, so multiple configs in the same repo do not collide. Notes are fetched from and pushed to the remote automatically. Pass `--no-cache` to `offload run` to skip reading and writing the cache; `--no-cache` preserves the same build procedure (tree export, base image build, thin diff) -- it only suppresses note interactions.

## Image Caching Modes

Offload uses a unified pipeline for image caching. Both modes follow identical steps after resolving the base commit: cache lookup, tree export, base image build, thin diff application, and note write. The only difference is how the base commit is selected.

### Parent-commit mode (default)

When no `[checkpoint]` section is present, Offload uses the parent commit (HEAD~1) as the base:

1. Look up HEAD~1 in git notes for a cached base image.
2. **Cache hit**: generate a binary diff from parent to HEAD and apply it on top of the cached image.
3. **Cache miss**: export the parent commit tree, build a base image from it, cache the result in git notes on HEAD~1, then apply thin diff.
4. **Initial commit** (no parent): fall through to a full build, no caching.

### Checkpoint mode (opt-in)

When a `[checkpoint]` section is present, Offload walks git ancestors to find the nearest commit that touched any `build_inputs` file. That commit is the base instead of HEAD~1. The rest of the pipeline (cache lookup, tree export, build, thin diff, note write) is the same as parent-commit mode.

Add a `[checkpoint]` section to `offload.toml`:

```toml
[checkpoint]
build_inputs = [
    "Dockerfile",
    "requirements.txt",
    "pyproject.toml",
]
```

`build_inputs` lists repo-relative file paths. A commit that modifies any of these files is automatically detected as a checkpoint. The list must be non-empty when the section is present. For merge commits, diffs against all parents are checked.

Enable checkpoint mode for repositories where dependency installation or build steps are expensive (e.g. `pip install`, `uv sync`, `cargo build`). Without checkpoints, every parent commit change requires a new base image build. With checkpoints, only commits that change dependency manifests trigger full rebuilds -- subsequent runs apply a lightweight diff on top of the cached checkpoint image.

### Thin diff details

The thin diff is a binary patch generated locally by Rust (`git diff --binary` plus untracked files detected via `git ls-files`). The patch file is passed to the Python script, which applies it inside the sandbox image via `git apply`. The Dockerfile must install `git` for this to work. If the diff is empty and there are no untracked files, the base image is used directly (zero overhead).

### Checking status

Use `offload checkpoint-status` to inspect the current checkpoint state:

```bash
offload checkpoint-status
```

Example output (with `[checkpoint]` section):

```
HEAD:               a1b2c3d4
Base commit:        f5e6d7c8 (checkpoint, 3 commits back)
Cached image:       im-abc123
Next run mode:      thin diff (2 files changed since checkpoint)
```

Example output (without `[checkpoint]` section, parent-commit mode):

```
HEAD:               a1b2c3d4
Base commit:        e5f6a7b8 (parent, HEAD~1)
Cached image:       im-abc123
Next run mode:      thin diff (2 files changed since parent)
```

This command works in both modes: with a `[checkpoint]` section it shows checkpoint info, without it shows parent-commit info.

### Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| Cached image expired | Modal garbage-collected the image | Self-healing: Offload rebuilds automatically on the next run, updates the note, and pushes. One slow run, then cached again for everyone |
| `git apply` failure inside sandbox | Diff cannot apply (e.g. missing `git` in image, conflicts) | Ensure the Dockerfile installs `git`. Offload falls back to a full build with a warning |
| No checkpoint found in last N commits | No recent commit touched any `build_inputs` file | The ancestor walk has a depth limit; a full build runs instead |
| Thin diff failure (general) | Various causes (binary incompatibility, corrupt patch) | Offload falls back to a full build with a warning. If persistent, run `--no-cache` to force a clean rebuild |
| Notes not shared across team | Remote does not have the notes ref | Notes are pushed automatically. Verify with `git ls-remote origin refs/notes/offload-images` |

## CLI Quick Reference

| Command | Purpose |
|---------|---------|
| `offload run` | Run tests in parallel |
| `offload collect` | Discover tests without running (supports `--format json`) |
| `offload validate` | Validate `offload.toml` and print settings summary |
| `offload init` | Generate a new `offload.toml` (`--provider`, `--framework`) |
| `offload logs` | View per-test results from the most recent run |
| `offload checkpoint-status` | Show checkpoint cache status for current HEAD |

Global flags: `-c, --config PATH` (config file), `-v, --verbose` (verbose output).

## Config Groups Reference

Groups partition tests for different retry policies and filter expressions. At least one group is required. Each group runs its own discovery pass.

```toml
[groups.unit]
retry_count = 0
filters = "-m 'not slow'"

[groups.slow]
retry_count = 1
filters = "-m slow"

[groups.flaky]
retry_count = 5
filters = "-k test_flaky"
```

- `filters` is passed to the framework during discovery (pytest args, nextest args, or substituted into `{filters}` for the default framework).
- `retry_count = 0` means no retries. Failed tests that pass on retry are marked flaky (exit code 2).
