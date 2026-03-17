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
| Slow sandbox creation | Docker image not cached | Delete `.offload-image-cache` or pass `--no-cache` |
| All tests fail with import errors | Sandbox missing dependencies | Check Dockerfile and `sandbox_init_cmd` |

## CLI Quick Reference

| Command | Purpose |
|---------|---------|
| `offload run` | Run tests in parallel |
| `offload collect` | Discover tests without running (supports `--format json`) |
| `offload validate` | Validate `offload.toml` and print settings summary |
| `offload init` | Generate a new `offload.toml` (`--provider`, `--framework`) |
| `offload logs` | View per-test results from the most recent run |

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
