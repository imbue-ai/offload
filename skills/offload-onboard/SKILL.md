---
name: offload-onboard
description: "Onboard a repository to use Offload for parallel test execution on Modal. Detects test setup, creates config, Dockerfile, CI job, and optimizes performance."
---

# Onboard Repository to Offload

This skill walks through onboarding the current repository to use **Offload** — a parallel test runner that executes tests across Modal cloud sandboxes. Offload is installed as part of the procedure below.

## Procedure

Follow these steps in order. Use the TodoWrite tool to track progress through each step.

### Step 1: Detect Test Framework and Test Paths

Investigate how the repository runs its tests:

1. Look for `pyproject.toml`, `setup.cfg`, `setup.py`, `Cargo.toml`, `package.json`, `go.mod`, or similar project files
2. Look for existing CI workflows (`.github/workflows/`, `.gitlab-ci.yml`, etc.) to see how tests are currently invoked
3. Look for test directories: `tests/`, `test/`, `**/test_*.py`, `**/*_test.go`, `src/**/tests/`, etc.
4. Determine:
   - **Framework**: `pytest`, `cargo`, `vitest`, or `default` (generic)
   - **Test paths**: Where tests live (e.g. `["tests/"]`, `["src/"]`)
   - **Python runner**: If pytest, determine if the project uses `uv`, `poetry`, `pip`, or plain `python`
   - **Extra args**: Any special invocation needed (e.g. `["run", "--group", "test"]` for uv with dependency groups)

Ask the user to confirm your detection if anything is ambiguous.

### Step 2: Verify Prerequisites

Verify the following are installed and authenticated. **Do not continue until all prerequisites are confirmed.**

- `uv` (**required** — Offload uses `uv` to run the Modal sandbox script regardless of project language or package manager)
- `modal` CLI — must be installed (`pip install modal`) **and** authenticated. Run `modal profile list` to check. If not authenticated, tell the user to run `modal token new` (opens a browser, writes credentials to `~/.modal.toml`). **Wait for the user to confirm authentication before proceeding.**
- For pytest projects: the configured Python runner (`uv`, `poetry`, or `python`) and pytest must be available locally for test discovery
- For cargo projects: `cargo-nextest` must be installed (`cargo install cargo-nextest`)

### Step 3: Find or Create a Dockerfile

Offload's Modal provider needs a Dockerfile to build sandbox images. Look for an existing one:

1. Check `.devcontainer/Dockerfile`, `Dockerfile`, `docker/Dockerfile`, or any Dockerfile referenced in CI
2. If one exists and is suitable (has the language runtime + package manager), note its path
3. If none exists, create `.devcontainer/Dockerfile` with the minimal base:

**For Python projects:**
```dockerfile
FROM python:<version>-slim

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
FROM rust:<version>-slim

RUN apt-get update && apt-get install -y --no-install-recommends git pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install cargo-nextest if the project uses it
# RUN cargo install cargo-nextest

WORKDIR /app
```

Key principles:
- Match the language/runtime version from the project's config (e.g. `requires-python`, `rust-version`)
- Include `git` if any tests create git repos or the project uses git-based dependencies
- Do NOT `COPY . .` — Offload overlays source via `--copy-dir` at image build time
- Keep it minimal — dependencies are installed at runtime inside the sandbox

### Step 4: Create offload.toml

Create `offload.toml` at the project root using the Modal provider. In the templates below, values in `<angle-brackets>` are placeholders you must substitute with project-specific values.

```toml
[offload]
max_parallel = 3
test_timeout_secs = 120
sandbox_project_root = "/app"

[provider]
type = "modal"
dockerfile = "<path-to-dockerfile>"
include_cwd = true

[framework]
type = "pytest"
paths = ["<test-paths>"]
command = "<pytest-command>"      # e.g. "uv run pytest", "poetry run pytest", "python -m pytest"

[groups.all]
retry_count = 0
filters = ""                    # pytest args for discovery filtering (e.g. "-m 'not slow'")

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

**For Cargo (Rust) projects**, replace the `[framework]` section with:

```toml
[framework]
type = "cargo"
```

**Optional: `sandbox_init_cmd` for build-time setup**

Do not add `sandbox_init_cmd` initially. If you observe that every sandbox runs the same setup work (e.g. `uv sync --all-packages`, `git apply`, database migrations), move that work into `sandbox_init_cmd` so it runs once during image build instead of on every sandbox:

```toml
[offload]
max_parallel = 3
test_timeout_secs = 120
sandbox_project_root = "/app"
sandbox_init_cmd = "uv sync --all-packages"   # runs once at image build time
```

This is especially useful for monorepo setups where dependency installation is slow.

**When to use `type = "default"` for the framework:**

The built-in `pytest` and `cargo` frameworks cover straightforward setups. Fall back to `type = "default"` for the `[framework]` section when:

- **Conflicting local config**: The project's `pyproject.toml` or `setup.cfg` has `addopts` that conflict with Offload (e.g. xdist workers, coverage plugins) and you need to override them with `-o addopts=` or `-p no:xdist`
- **Custom discovery pipeline**: Standard collection doesn't work and you need shell pipelines (e.g. grep filtering, marker exclusions combined with workspace sync)
- **Unsupported framework**: Jest, Go, Mocha, or any framework not directly supported

Example — pytest in a monorepo with xdist conflict (still using the Modal provider):

```toml
[provider]
type = "modal"
dockerfile = "<path-to-dockerfile>"
include_cwd = true

[framework]
type = "default"
discover_command = "uv run pytest --collect-only -q {filters} 2>/dev/null | grep '::'"
run_command = "cd /app && uv run pytest -v --tb=short --no-cov -p no:xdist -o addopts= --junitxml={result_file} {tests}"
test_id_format = "{name}"
```

Note: if discovery or execution requires pre-steps like `uv sync --all-packages`, use `sandbox_init_cmd` in the `[offload]` section rather than inlining them into the discover/run commands.

For the full configuration reference and more examples, see the Offload README.

Configuration reference for fields used above:

**`[offload]`**
- `max_parallel`: Number of concurrent Modal sandboxes (start with 3, optimize later)
- `test_timeout_secs`: Per-test-batch timeout in seconds (120s is generous for unit tests)
- `sandbox_project_root`: Project root path inside the sandbox, exported as `OFFLOAD_ROOT`
- `sandbox_init_cmd`: Optional command to run during image build, after cwd/copy-dirs are applied (e.g. `"uv sync --all-packages"`)

**`[provider]`** (Modal)
- `dockerfile`: Path to the Dockerfile for building the sandbox image
- `include_cwd`: Copy the current working directory into the image (default: `false`)

**`[framework]`** (pytest)
- `paths`: Directories to search for tests (default: `["tests"]`)
- `command`: Full command prefix for pytest invocation (e.g. `"uv run pytest"`). Replaces the legacy `python`/`extra_args` fields

**`[groups.<name>]`**
- `retry_count`: Number of retries for failed tests (0 = no retries, 1 = catches transient failures)
- `filters`: Filter string passed to the framework during discovery (e.g. `-m 'not slow'`)

**IMPORTANT: Do not use JUnit reporting plugins or hooks.** Offload owns the JUnit XML report — it passes `--junitxml` to pytest and uses the resulting XML as the single source of truth for matching test results back to discovered tests. Custom conftest hooks, plugins, or pytest options that modify JUnit XML output (e.g. `record_xml_attribute`, `pytest-metadata`, or `junit_family` overrides) will interfere with Offload's test ID resolution and cause incorrect results. If the project has existing JUnit customization in conftest.py or pytest plugins, **remove it** before proceeding.

### Step 5: Verify Vitest Test Discovery (vitest only)

**Skip this step if the framework is not `vitest`.**

Offload's vitest integration uses `--testNamePattern` to select specific tests for parallel execution. This flag matches tests by their **space-separated** describe/test chain — not by file path. When two tests in different files share the same space-separated name, Offload cannot distinguish them and **will error during discovery**, blocking all test execution.

Run `offload collect` to verify that test discovery works:

```bash
offload collect
```

If Offload reports **"Duplicate test names found (space-separated)"**, stop and present the duplicates to the user. Explain:

> Offload cannot run vitest tests that share the same space-separated name across different files. This is a requirement of Offload's `--testNamePattern`-based test selection — without unique names, Offload will error during test discovery and no tests will run.

Then ask: **"Would you like me to deduplicate these test names by making them more descriptive? This typically involves wrapping tests in uniquely named `describe()` blocks or renaming `test()`/`it()` calls to include more context."**

If the user agrees, rename the tests to make the space-separated names unique. Prefer wrapping in descriptive `describe()` blocks over renaming individual tests, as this preserves the existing test names while adding disambiguation.

After making changes, run `offload collect` again. **Repeat until `offload collect` succeeds with no duplicate name errors.**

If the user declines deduplication, **do not proceed with Offload onboarding** — inform them that Offload's vitest integration cannot function with duplicate test names and they will need to resolve this before onboarding can continue.

**Additional check: tests with literal `>` in their names.**

After `offload collect` passes, run `npx vitest list --json` and check for test names where a ` > `-separated segment itself contains `>` (e.g. `describe('parse') > it('attribute value with >')`). Offload uses ` > ` as the separator between the file path and the describe/test chain in test IDs, so a literal `>` in a test name creates ambiguity that causes "Not Run" results.

If any are found, present them to the user and recommend renaming the affected `test()`/`it()`/`describe()` calls to avoid `>` characters. For example, rename `'attribute value with >'` to `'attribute value with greater-than'`. This is not a blocking error — `offload collect` will still pass — but affected tests will be reported as "Not Run" after execution because Offload cannot match their results back to discovered IDs.

### Step 6: Create Local Invocation Script

Create `scripts/offload-tests.sh`:

```bash
#!/usr/bin/env bash
#
# Run the project's test suite via Offload (parallel on Modal).
# Requires: Offload (cargo install offload), Modal CLI + credentials
#
set -euo pipefail

if ! command -v offload &> /dev/null; then
    echo "Error: 'offload' not installed. Install with: cargo install offload"
    exit 1
fi

cd "$(git rev-parse --show-toplevel)"
exec offload run --copy-dir ".:/app" "$@"
```

Make it executable with `chmod +x scripts/offload-tests.sh`.

The `--copy-dir` flag tells Offload to bake the local directory into the sandbox image at the given path during the prepare step. The target path must match `sandbox_project_root` in `offload.toml` (e.g. `".:/app"` when `sandbox_project_root = "/app"`). This is specified at invocation time, not in `offload.toml`, because it depends on where you're running from.

**Use this script (or the equivalent invocation) for all subsequent steps that run Offload.**

If the project uses a Makefile, justfile, or Taskfile instead of scripts/, add the invocation there instead to be consistent with existing practice.

### Step 7: Update .gitignore

Append Offload artifacts to `.gitignore`:

```
# Offload
test-results/
```

NOTE: `.offload-image-cache` should be checked in to git — it tracks the base image ID and speeds up subsequent runs. Do not confuse `.gitignore` (which controls what git tracks) with `.dockerignore` (which controls what gets copied into the sandbox image). The `.dockerignore` is only created if needed during troubleshooting — see the Troubleshooting section.

### Step 8: Run Offload Locally and Verify

Install offload if not already present:

```bash
cargo install offload
```

Run the tests using the invocation script from Step 6:

**First, verify test discovery with `offload collect`:**

```bash
offload collect
```

This runs test discovery locally without creating sandboxes or executing tests. Fix any errors until `offload collect` succeeds and lists the expected tests. Common issues:

1. **"No tests discovered"**: Check that `paths` in `offload.toml` points to the correct test directories and the framework command is correct.
2. **"Duplicate test names found"** (vitest): Duplicate space-separated test names exist. Return to Step 5 and resolve them.
3. **Discovery command failed**: The framework tool (`pytest`, `cargo nextest`, `npx vitest`) is not installed or not on PATH.

Do not proceed to execution until `offload collect` lists the expected tests.

**Then, establish a baseline by running the test suite directly (without Offload):**

```bash
# For vitest:
pnpm exec vitest run --project <project>
# For pytest:
<command> --tb=short
# For cargo:
cargo nextest run
```

Record the number of passed, failed, and skipped tests. Some projects have pre-existing test failures (snapshot mismatches, missing fixtures, flaky tests). These are **not your problem to fix** — the goal is for Offload's results to match this baseline, not to achieve zero failures.

**Then, run the full test suite via Offload:**

```bash
./scripts/offload-tests.sh
```

**Compare Offload's results against the baseline.** Offload is working correctly when:
- The number of passed tests matches the baseline (within 1-2 for flaky tests)
- The number of failed tests matches the baseline
- There are no "Not Run" tests (0 missing)

**If Offload's results differ from the baseline, iterate until they match.** Common issues and fixes:

1. **"Exec format error"**: `.venv` or local binaries leaked into the sandbox. See the Troubleshooting section on creating a `.dockerignore`.
2. **"No such file or directory"**: The sandbox is missing a dependency. Check the Dockerfile has the right runtime and package manager.
3. **"Token validation failed"**: Modal credentials are expired. Run `modal token new` to refresh.
4. **Tests discovered but "Not Run"**: The test command is failing silently inside the sandbox. Check batch logs in `test-results/logs/` for errors. Debug by checking if `uv`/`python`/`cargo` is available in the Dockerfile.
5. **More failures than baseline**: Offload may be running tests in a different order or with different environment. Check batch logs for the extra failures and compare with the baseline output.
6. **Fewer tests discovered than expected**: Check that `offload collect` finds all the tests. If using vitest with `--project`, ensure the filter matches.

**Keep running and fixing until Offload's pass/fail counts match the baseline. Do not proceed to optimization until they match.**

### Step 9: Optimize Parallelism

Run a simple linear search over `max_parallel` to minimize total runtime:

1. Test `max_parallel` values: 1, 2, 3, 4, 6, 8 (keeping other params fixed)
2. For each value, edit `offload.toml`, run `time ./scripts/offload-tests.sh`, and record the wall-clock duration from the `real` line
3. Pick the value with the lowest duration
4. Optionally test `retry_count = 0` vs `retry_count = 1` at the optimal parallelism

The optimal `max_parallel` depends on the number of test files and per-test duration. More parallelism has diminishing returns due to sandbox creation overhead.

Report the results as a table to the user and set the optimal values in `offload.toml`.

### Step 11: Enable Checkpoint Mode (optional)

**Skip this step if the project has fast dependency installs (under ~30 seconds).** Checkpoint mode is most useful for repositories where dependency installation or build steps are expensive (e.g. large `requirements.txt`, monorepo `uv sync --all-packages`, heavy `cargo build`).

To enable checkpoint mode, add a `[checkpoint]` section to `offload.toml` listing the files whose changes should trigger a full image rebuild:

```toml
[checkpoint]
build_inputs = [
    "Dockerfile",
    "requirements.txt",
    "pyproject.toml",
]
```

Common `build_inputs` patterns:

| Project type | Typical build_inputs |
|-------------|---------------------|
| Python (pip/uv) | `Dockerfile`, `requirements.txt`, `pyproject.toml`, `uv.lock` |
| Python (poetry) | `Dockerfile`, `pyproject.toml`, `poetry.lock` |
| Rust | `Dockerfile`, `Cargo.toml`, `Cargo.lock` |
| Node.js | `Dockerfile`, `package.json`, `package-lock.json` |

After adding the section, run `offload validate` to check the config and `offload checkpoint-status` to verify checkpoint detection.

This step is optional. Repositories without a `[checkpoint]` section still benefit from per-commit image caching via git notes -- every `offload run` caches its image so that repeated runs against the same commit reuse the cached image.

### Step 12: Update Agent/Project Instructions (if desired)

**First, ask the user:** "Would you like to configure Offload as the default test runner for AI agents working in this repository? This requires agents to have access to Modal API credentials."

**If the user declines**, skip this step entirely and proceed to Step 13.

**If the user agrees**, ensure that future AI agents working in this repository know to use Offload for running tests:

1. Check for these files (in priority order):
   - `CLAUDE.md` or `.claude/CLAUDE.md` — Claude Code project instructions
   - `AGENTS.md` — Multi-agent coordination instructions
   - `CONTRIBUTING.md` — Contributor guidelines

2. **If at least one exists**, read it to understand its structure and find any existing section about running tests (look for headings or instructions mentioning "test", "testing", "verification", "CI", "check").

3. **If both `CLAUDE.md` and `AGENTS.md` exist**, update both. If only one exists, update that one.

4. **If none of these files exist**, create a `CLAUDE.md` at the project root. It only needs the testing section — don't fabricate other content.

5. **Amend or add** a testing section that is directive, not merely suggestive. The instruction must tell agents to use Offload as the way to run tests locally. Do not remove any existing test commands — keep them as a fallback — but make Offload the primary instruction. The section should also reference the `/offload` skill so agents activate it when running tests, reading logs, or debugging failures. Example:

   ````markdown
   ## Running tests

   Run the test suite via Offload, which parallelizes execution across Modal cloud sandboxes:

   ```bash
   ./scripts/offload-tests.sh
   ```

   Prerequisites: Offload (`cargo install offload`) and Modal credentials (`modal token new`).
   Activate the `/offload` skill for test execution, log reading, and failure debugging.
   ````

   Adapt the exact command to match what was configured in earlier steps (the script path, etc.).

6. Preserve the existing tone and formatting of the file. If it uses a digraph, bullet lists, or a specific heading style, match that style. Do not restructure or reformat existing content.

### Step 13: Set Up CI Job (if desired)

Ask the user if they want to set up a CI job to run Offload tests automatically on push/PR. If they decline, skip Steps 13 and 14.

If they want CI, detect the CI system from the repository:
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
          python-version: "<version>"

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
          key: cargo-offload-${{ runner.os }}

      - name: Install offload
        run: |
          if ! command -v offload &> /dev/null; then
            cargo install offload
          fi

      - name: Install Modal CLI
        run: pip install modal

      - name: Run tests via offload
        env:
          MODAL_TOKEN_ID: ${{ secrets.MODAL_TOKEN_ID }}
          MODAL_TOKEN_SECRET: ${{ secrets.MODAL_TOKEN_SECRET }}
        run: offload run --copy-dir ".:/app"  # adjust path to match sandbox_project_root
```

**IMPORTANT**: The CI runner needs the project's language toolchain and dependencies installed because Offload discovers tests **locally** (e.g. `uv -m pytest --collect-only`), then sends them to Modal for execution. Without local discovery dependencies, Offload will fail with "No such file or directory".

`continue-on-error: true` makes the job advisory — it always reports success to branch protection, but step-level pass/fail is visible in the Actions UI.

For other CI systems, adapt the same pattern: install Offload + Modal CLI, set Modal secrets as env vars, run `offload run`.

### Step 14: Configure CI Secrets

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
| "Exec format error (os error 8)" | Local `.venv` (macOS/Windows binaries) copied into Linux sandbox | Create a `.dockerignore` (see below) |
| "Token validation failed" | Modal credentials expired | `modal token new` |
| All tests "Not Run" / junit.xml missing | Test command failing inside sandbox | Check Dockerfile has correct runtime; debug with `modal sandbox create` |
| "No such file or directory" on CI | Missing local discovery dependencies | Add language toolchain + dep install steps before Offload |
| Slow sandbox creation | Docker image not cached | Run once to warm cache; `.offload-image-cache` tracks the base image ID |
| Stale sandbox image | `.offload-image-cache` points to an outdated image | Delete `.offload-image-cache` to force a fresh image build on next run |
| High parallelism slower than low | Sandbox creation overhead dominates | Reduce `max_parallel`; optimal is usually 2-6 for small test suites |
| Tests fail with unexpected errors in sandbox | Local artifacts (caches, build dirs) interfere with sandbox environment | Create a `.dockerignore` (see below) |

### Creating a .dockerignore

If tests fail due to local artifacts leaking into the sandbox (e.g. "Exec format error" from a macOS `.venv` copied into a Linux sandbox, or stale `__pycache__`/build directories causing conflicts), create a `.dockerignore` at the project root to exclude them:

```
.venv
.git
.github
__pycache__
*.egg-info
.offload-image-cache  # excluded from Docker build context, but should be checked in to git
test-results
build
dist
target
node_modules
```

**CRITICAL**: `.venv` is the most common culprit. If a local virtual environment (e.g. macOS binaries) gets copied into a Linux sandbox, tests will fail with "Exec format error". This is the most common onboarding failure.

**NOTE**: Sometimes tests depend on the git repository. If tests fail because `.git` is missing, remove `.git` from the `.dockerignore`.

## Summary of Files Created/Modified

| File | Purpose |
|------|---------|
| `.devcontainer/Dockerfile` (or existing) | Base image for Modal sandboxes |
| `.dockerignore` | (If needed) Exclude local artifacts from sandbox — see Troubleshooting |
| `offload.toml` | Offload configuration |
| `scripts/offload-tests.sh` (or Makefile target) | Local invocation convenience |
| `.gitignore` | Exclude Offload artifacts |
| `CLAUDE.md` / `AGENTS.md` | (Optional) Add or create directive Offload test instructions for agents |
| `.github/workflows/test-offload.yml` (or equivalent) | Advisory CI job |
