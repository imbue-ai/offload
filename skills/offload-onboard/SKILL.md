---
name: offload-onboard
description: "Onboard a repository to use offload for parallel test execution on Modal. Detects test setup, creates config, Dockerfile, CI job, and optimizes performance."
---

# Onboard Repository to Offload

This skill walks through onboarding the current repository to use **offload** — a parallel test runner that executes tests across Modal cloud sandboxes.

**Install offload**: `cargo install offload@0.3.0`

## Procedure

Follow these steps in order. Use the TodoWrite tool to track progress through each step.

### Step 1: Detect Test Framework and Test Paths

Investigate how the repository runs its tests:

1. Look for `pyproject.toml`, `setup.cfg`, `setup.py`, `Cargo.toml`, `package.json`, `go.mod`, or similar project files
2. Look for existing CI workflows (`.github/workflows/`, `.gitlab-ci.yml`, etc.) to see how tests are currently invoked
3. Look for test directories: `tests/`, `test/`, `**/test_*.py`, `**/*_test.go`, `src/**/tests/`, etc.
4. Determine:
   - **Framework**: `pytest`, `cargo`, or `default` (generic)
   - **Test paths**: Where tests live (e.g. `["tests/"]`, `["src/"]`)
   - **Python runner**: If pytest, determine if the project uses `uv`, `poetry`, `pip`, or plain `python`
   - **Extra args**: Any special invocation needed (e.g. `["run", "--group", "test"]` for uv with dependency groups)

Ask the user to confirm your detection if anything is ambiguous.

### Step 2: Find or Create a Dockerfile

Offload's Modal provider needs a Dockerfile to build sandbox images. Look for an existing one:

1. Check `.devcontainer/Dockerfile`, `Dockerfile`, `docker/Dockerfile`, or any Dockerfile referenced in CI
2. If one exists and is suitable (has the language runtime + package manager), note its path
3. If none exists, create `.devcontainer/Dockerfile` with the minimal base:

**For Python projects:**
```dockerfile
FROM python:3.XX-slim

RUN apt-get update && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*

# Install the package manager used by the project (uv, poetry, etc.)
# For uv:
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
# For poetry:
# RUN pip install poetry

WORKDIR /app
```

**For Rust projects:**
```dockerfile
FROM rust:1.XX-slim

RUN apt-get update && apt-get install -y --no-install-recommends git pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install cargo-nextest if the project uses it
# RUN cargo install cargo-nextest

WORKDIR /app
```

Key principles:
- Match the language/runtime version from the project's config (e.g. `requires-python`, `rust-version`)
- Include `git` if any tests create git repos or the project uses git-based dependencies
- Do NOT `COPY . .` — offload overlays source via `--copy-dir` at image build time
- Keep it minimal — dependencies are installed at runtime inside the sandbox

### Step 3: Create .dockerignore

Create a `.dockerignore` to prevent local artifacts from being copied into sandboxes:

```
.venv
.git
.github
__pycache__
*.egg-info
.offload
.offload-image-cache
.offload-cache
test-results
build
dist
target
node_modules
```

**CRITICAL**: `.venv` must be excluded. If a local virtual environment (e.g. macOS binaries) gets copied into a Linux sandbox, tests will fail with "Exec format error". This is the most common onboarding failure.
**NOTE**: Sometimes tests depend on the git repository, and we will need to include `.git`. Attempt to begin by removing it, and only include it if necessary.

### Step 4: Create offload.toml

Ask the user for a Modal app name (suggest `{repo-name}-tests`).

Create `offload.toml` at the project root. Start with these defaults:

```toml
[offload]
max_parallel = 3
test_timeout_secs = 120
stream_output = true

[provider]
type = "modal"
app_name = "{app-name}"
dockerfile = "{path-to-dockerfile}"
working_dir = "/app"
timeout_secs = 300

[groups.all]
type = "{framework}"
paths = ["{test-paths}"]
python = "{runner}"           # pytest only
extra_args = ["{extra-args}"] # if needed
retry_count = 0

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

Configuration reference:
- `max_parallel`: Number of concurrent Modal sandboxes (start with 3, optimize later)
- `test_timeout_secs`: Per-test-batch timeout (120s is generous for unit tests)
- `timeout_secs`: Provider-level sandbox lifetime (300s covers setup + test + teardown)
- `retry_count`: Number of retries for failed tests (0 = no retries, 1 = catches transient failures)

### Step 5: Add JUnit ID Normalization (pytest only)

**Skip this step if the framework is not `pytest`.**

By default, pytest's JUnit XML output uses a `classname` + `name` format that cannot be losslessly converted back to the original nodeid used during test collection. This causes offload to report tests as "Not Run" because it cannot match JUnit results to discovered tests.

Add an autouse fixture that writes the full nodeid into the JUnit `name` attribute. Offload's parser already handles `name` values containing `::` by using them verbatim.

1. Identify the root `conftest.py` for the test paths configured in `offload.toml` (e.g., `tests/conftest.py`)
2. If a `conftest.py` already exists at that location, check whether it already contains `_offload_junit_nodeid` or an equivalent `record_xml_attribute("name", ...)` override. If so, skip.
3. If no `conftest.py` exists, create one. If one exists, append to it.

Add the following fixture:

```python
import pytest


@pytest.fixture(autouse=True)
def _offload_junit_nodeid(record_xml_attribute, request):
    """Override JUnit name to use the full nodeid, matching pytest --collect-only output.

    Offload relies on matching JUnit test IDs to collected test IDs.  When the JUnit
    ``name`` attribute contains ``::`` offload uses it verbatim, bypassing the lossy
    classname reconstruction.  This fixture writes the full nodeid into ``name`` so
    the IDs always match.
    """
    record_xml_attribute("name", request.node.nodeid)
```

Additionally, ensure `junit_family = "xunit1"` is set in the project's pytest configuration. The `record_xml_attribute` fixture is incompatible with the default `xunit2` family. Add it to whichever config file the project uses:

- **pyproject.toml**: Under `[tool.pytest.ini_options]`, add `junit_family = "xunit1"`
- **pytest.ini**: Add `junit_family = xunit1` under `[pytest]`
- **setup.cfg**: Add `junit_family = xunit1` under `[tool:pytest]`

Ensure the `import pytest` line is not duplicated if the file already imports it.

### Step 6: Create Local Invocation Script

Create `scripts/offload-tests.sh`:

```bash
#!/usr/bin/env bash
#
# Run the project's test suite via offload (parallel on Modal).
# Requires: offload (cargo install offload@0.3.0), Modal CLI + credentials
#
set -euo pipefail

if ! command -v offload &> /dev/null; then
    echo "Error: 'offload' not installed. Install with: cargo install offload@0.3.0"
    exit 1
fi

cd "$(git rev-parse --show-toplevel)"
exec offload run --copy-dir ".:/app" "$@"
```

Make it executable with `chmod +x scripts/offload-tests.sh`.

The `--copy-dir ".:/app"` flag tells offload to copy the current directory into `/app` in the sandbox. This is specified at invocation time (not in offload.toml) because it's a runtime concern.

If the project uses a Makefile, justfile, or Taskfile instead of scripts/, add the invocation there instead to be consistent with existing practice.

### Step 7: Update .gitignore

Append offload artifacts to `.gitignore`:

```
# Offload
test-results/
.offload-cache/
```

### Step 8: Authenticate with Modal

Check if Modal credentials are configured:

```bash
modal profile list
```

If not authenticated, tell the user:
1. Install the Modal CLI: `pip install modal`
2. Create an account and authenticate: `modal token new`
3. This opens a browser for authentication and writes credentials to `~/.modal.toml`

Wait for the user to confirm they've authenticated before proceeding.

### Step 9: Run Offload Locally and Verify

Install offload if not already present:

```bash
cargo install offload@0.3.0
```

Run the tests:

```bash
offload run --copy-dir ".:/app"
```

**All tests must pass.** If they don't:

1. **"Exec format error"**: `.venv` or local binaries leaked into the sandbox. Ensure `.dockerignore` excludes `.venv`.
2. **"No such file or directory"**: The sandbox is missing a dependency. Check the Dockerfile has the right runtime and package manager.
3. **"Token validation failed"**: Modal credentials are expired. Run `modal token new` to refresh.
4. **Tests discovered but "Not Run"**: The test command is failing silently inside the sandbox. Debug by checking if `uv`/`python`/`cargo` is available in the Dockerfile.

Do not proceed to optimization until all tests pass.

### Step 10: Optimize Parameters via Gradient Descent

Run a simple linear search over `max_parallel` to minimize total runtime:

1. Test `max_parallel` values: 1, 2, 3, 4, 6, 8 (keeping other params fixed)
2. For each value, edit `offload.toml`, run `offload run --copy-dir ".:/app"`, and record the duration
3. Pick the value with the lowest duration
4. Optionally test `retry_count = 0` vs `retry_count = 1` at the optimal parallelism

The optimal `max_parallel` depends on the number of test files and per-test duration. More parallelism has diminishing returns due to sandbox creation overhead.

Report the results as a table to the user and set the optimal values in `offload.toml`.

### Step 11: Update Agent/Project Instructions (Optional)

**First, ask the user:** "Would you like to configure offload as the default test runner for AI agents working in this repository? This requires agents to have access to Modal API credentials."

**If the user declines**, skip this step entirely and proceed to Step 12.

**If the user agrees**, ensure that future AI agents working in this repository know to use offload for running tests:

1. Check for these files (in priority order):
   - `CLAUDE.md` or `.claude/CLAUDE.md` — Claude Code project instructions
   - `AGENTS.md` — Multi-agent coordination instructions
   - `CONTRIBUTING.md` — Contributor guidelines

2. **If at least one exists**, read it to understand its structure and find any existing section about running tests (look for headings or instructions mentioning "test", "testing", "verification", "CI", "check").

3. **If both `CLAUDE.md` and `AGENTS.md` exist**, update both. If only one exists, update that one.

4. **If none of these files exist**, create a `CLAUDE.md` at the project root. It only needs the testing section — don't fabricate other content.

5. **Amend or add** a testing section that is directive, not merely suggestive. The instruction must tell agents to use offload as the way to run tests locally. Do not remove any existing test commands — keep them as a fallback — but make offload the primary instruction. Example:

   ````markdown
   ## Running tests

   Run the test suite via offload, which parallelizes execution across Modal cloud sandboxes:

   ```bash
   ./scripts/offload-tests.sh
   ```

   Or directly:

   ```bash
   offload run --copy-dir ".:/app"
   ```

   Prerequisites: offload (`cargo install offload@0.3.0`) and Modal credentials (`modal token new`).
   ````

   Adapt the exact command to match what was configured in earlier steps (the script path, `--copy-dir` mapping, etc.).

6. Preserve the existing tone and formatting of the file. If it uses a digraph, bullet lists, or a specific heading style, match that style. Do not restructure or reformat existing content.

### Step 12: Set Up CI Job (if applicable)

Detect the CI system from the repository:
- `.github/workflows/` → GitHub Actions
- `.gitlab-ci.yml` → GitLab CI
- `Jenkinsfile` → Jenkins
- `.circleci/` → CircleCI

If no CI system is detected, inform the user and skip this step.

**For GitHub Actions**, create `.github/workflows/test-offload.yml`:

```yaml
name: Offload Tests

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  workflow_dispatch:

jobs:
  offload-tests:
    runs-on: ubuntu-latest
    continue-on-error: true   # Advisory only - never blocks merging
    steps:
      - uses: actions/checkout@v4

      - name: Set up Python
        uses: actions/setup-python@v5
        with:
          python-version: "3.XX"

      # Include language-specific setup needed for LOCAL test discovery
      # offload discovers tests locally, then executes them remotely
      # Example for uv-based Python project:
      - name: Install uv
        uses: astral-sh/setup-uv@v5

      - name: Install dependencies
        run: uv sync --all-groups

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache offload binary
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            ~/.cargo/bin/offload
          key: cargo-offload-0.3.0-${{ runner.os }}

      - name: Install offload
        run: |
          if ! command -v offload &> /dev/null; then
            cargo install offload@0.3.0
          fi

      - name: Install Modal CLI
        run: pip install modal

      - name: Run tests via offload
        env:
          MODAL_TOKEN_ID: ${{ secrets.MODAL_TOKEN_ID }}
          MODAL_TOKEN_SECRET: ${{ secrets.MODAL_TOKEN_SECRET }}
        run: offload run --copy-dir ".:/app"
```

**IMPORTANT**: The CI runner needs the project's language toolchain and dependencies installed because offload discovers tests **locally** (e.g. `uv -m pytest --collect-only`), then sends them to Modal for execution. Without local discovery dependencies, offload will fail with "No such file or directory".

`continue-on-error: true` makes the job advisory — it always reports success to branch protection, but step-level pass/fail is visible in the Actions UI.

For other CI systems, adapt the same pattern: install offload + Modal CLI, set Modal secrets as env vars, run `offload run --copy-dir ".:/app"`.

### Step 13: Configure CI Secrets

Tell the user they need to add two repository secrets:
- `MODAL_TOKEN_ID`: Their Modal API token ID
- `MODAL_TOKEN_SECRET`: Their Modal API token secret

These can be found in `~/.modal.toml` after running `modal token new`.

**For GitHub**: Settings → Secrets and variables → Actions → New repository secret

Offer to trigger a CI run (if GitHub Actions) once the user confirms secrets are configured:

```bash
gh workflow run test-offload.yml
gh run list --workflow=test-offload.yml --limit=1
gh run watch <run-id> --exit-status
```

Wait for the run to succeed. If it fails, diagnose and fix the issue, then trigger again.

## Troubleshooting Reference

| Symptom | Cause | Fix |
|---------|-------|-----|
| "Exec format error (os error 8)" | Local `.venv` (macOS/Windows binaries) copied into Linux sandbox | Add `.venv` to `.dockerignore` |
| "Token validation failed" | Modal credentials expired | `modal token new` |
| All tests "Not Run" / junit.xml missing | Test command failing inside sandbox | Check Dockerfile has correct runtime; debug with `modal sandbox create` |
| "No such file or directory" on CI | Missing local discovery dependencies | Add language toolchain + dep install steps before offload |
| Slow sandbox creation | Docker image not cached | Run once to warm cache; `.offload-image-cache` tracks the base image ID |
| High parallelism slower than low | Sandbox creation overhead dominates | Reduce `max_parallel`; optimal is usually 2-6 for small test suites |

## Summary of Files Created/Modified

| File | Purpose |
|------|---------|
| `.devcontainer/Dockerfile` (or existing) | Base image for Modal sandboxes |
| `.dockerignore` | Exclude local artifacts from sandbox |
| `offload.toml` | Offload configuration |
| `scripts/offload-tests.sh` (or Makefile target) | Local invocation convenience |
| `.gitignore` | Exclude offload artifacts |
| `CLAUDE.md` / `AGENTS.md` | (Optional) Add or create directive offload test instructions for agents |
| `.github/workflows/test-offload.yml` (or equivalent) | Advisory CI job |
