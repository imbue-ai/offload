# Offload

A flexible parallel test runner written in Rust with pluggable execution providers. By [Imbue AI](https://github.com/imbue-ai).

## Features

- **Parallel execution** across multiple sandboxes (local processes or remote environments)
- **Pluggable providers**: local, default (custom shell commands), and Modal
- **Multiple test frameworks**: pytest, cargo test, or any custom runner
- **Automatic retry** with flaky test detection
- **JUnit XML** reporting
- **LPT scheduling** when historical timing data is available, with round-robin fallback
- **Group-level filtering** to split tests into groups with different filters and retry policies
- **Environment variable expansion** in config values (`${VAR}` and `${VAR:-default}`)
- **Bundled script references** using `@filename.ext` syntax in commands

## Installation

From crates.io:

```bash
cargo install offload@0.4.0
```

From source:

```bash
cargo install --path .
```

## Prerequisites

**Core:**
- Rust toolchain (`cargo`) to install Offload

**For Modal providers** (`type = "modal"` or `type = "default"` with `@modal_sandbox.py`):
- [uv](https://docs.astral.sh/uv/) — the bundled `modal_sandbox.py` is invoked via `uv run`, which auto-installs its dependencies (`modal`, `click`)
- Python >= 3.10
- A Modal account — authenticate with `modal token new`

**For the pytest framework** (local test discovery):
- Python and pytest installed locally — Offload runs `pytest --collect-only` on the local machine to discover tests
- The configured Python runner (e.g. `uv`, `poetry`, `python`) must be on PATH

**For the cargo framework:**
- [cargo-nextest](https://nexte.st/) — Offload runs `cargo nextest list` for test discovery. Install with `cargo install cargo-nextest`

**For the default framework:**
- Whatever tools your `discover_command` and `run_command` invoke

## Invariants and Expectations

Offload relies on a stable relationship between test discovery, execution, and result reporting. Understanding these expectations is essential when using the `default` framework or debugging test ID mismatches.

### Discovery

Each group triggers its own discovery call. The discovered test IDs become the canonical identifiers for the entire run.

- **pytest**: Runs `{python} -m pytest --collect-only -q` locally and parses one test ID per line from stdout. Output format: `path/to/test.py::TestClass::test_method`. Group `filters` are appended as extra pytest args (e.g. `-m 'not slow'`).
- **cargo**: Runs `cargo nextest list --message-format json` locally and parses test IDs from the JSON output. Test IDs are formatted as `{binary_id} {test_name}`. Group `filters` are appended as extra nextest args.
- **default**: Runs `discover_command` through `sh -c` and reads one test ID per line from stdout. The `{filters}` placeholder is replaced with the group's filter string (or empty string). Lines starting with `#` are ignored.

### Test ID Matching

Offload matches discovered test IDs to JUnit XML results using a `test_id_format` string that controls how JUnit XML `name` and `classname` attributes are combined into a test ID. For example, `"{name}"` uses just the name attribute; `"{classname} {name}"` joins them with a space. This is the most common source of "Not Run" errors.

- The JUnit attributes produced by the test runner **must match** the test ID from discovery after applying `test_id_format`. If they don't match, Offload reports the test as "Not Run".
- **pytest**: The format is fixed internally as `"{name}"`. The `_set_junit_test_id` conftest fixture writes the full nodeid into the JUnit `name` attribute so it matches the `pytest --collect-only` output. Users do not configure this field.
- **cargo**: The format is fixed internally as `"{classname} {name}"` where classname is the binary ID and name is the test function. Users do not configure this field.
- **default**: The `test_id_format` field is a required configuration option. Set it to match how your test runner populates the JUnit XML `name` and `classname` attributes.

### Result Reporting

After execution, Offload collects results via one of two mechanisms:

- **JUnit XML** (recommended): The test command writes a JUnit XML file. For the `default` framework, configure `result_file` with the path and use `{result_file}` in `run_command`. For pytest and cargo, Offload generates the `--junitxml` / nextest JUnit flags automatically.
- **Exit code fallback** (default framework only): If no `result_file` is configured, Offload infers pass/fail from the command's exit code. This loses per-test granularity — all tests are reported under a synthetic `all_tests` ID, and flaky test detection will not work.

### Retry and Flaky Test Behavior

- Tests are retried up to `retry_count` times (configured per group).
- Retries run in parallel across available sandboxes.
- If **any** retry attempt passes, the test is reported as passed.
- A test that passes after a failure is marked as **flaky** (exit code 2).
- Without JUnit XML result files, retries cannot identify individual test failures and may behave incorrectly.

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All tests passed |
| 1 | One or more tests failed, or tests were not run |
| 2 | All tests passed, but some were flaky (passed only on retry) |

### Output Files

After a test run, Offload writes per-batch log files to `{output_dir}/logs/`:

| File | Meaning |
|------|---------|
| `batch-{N}.success` | All tests in batch N passed |
| `batch-{N}.failure` | One or more tests in batch N failed |
| `batch-{N}.error` | Infrastructure error (e.g., sandbox failure, JUnit download failure) |
| `batch-{N}.cancelled` | Batch was cancelled before completion (e.g., early stopping) |

Each file contains stdout and stderr from all tests in that batch. Stderr lines are prefixed with `[stderr]`.

The `{output_dir}` defaults to `test-results` and is configurable via `[report] output_dir`.

## Quick Start

1. Initialize a configuration file:

```bash
offload init --provider local --framework pytest
```

2. Edit `offload.toml` as needed for your project.

3. Run tests:

```bash
offload run
```

## CLI Reference

### Global Flags

| Flag | Description |
|------|-------------|
| `-c, --config PATH` | Configuration file path (default: `offload.toml`) |
| `-v, --verbose` | Enable verbose output |

### `offload run`

Run tests in parallel.

| Flag | Description |
|------|-------------|
| `--parallel N` | Override maximum parallel sandboxes |
| `--collect-only` | Discover tests without running them |
| `--copy-dir LOCAL:REMOTE` | Copy a directory into each sandbox (repeatable) |
| `--env KEY=VALUE` | Set an environment variable in sandboxes (repeatable) |
| `--no-cache` | Skip cached image lookup during prepare (forces fresh build) |

### `offload collect`

Discover tests without running them.

| Flag | Description |
|------|-------------|
| `-f, --format text\|json` | Output format (default: `text`) |

### `offload validate`

Validate the configuration file and print a summary of settings.

### `offload init`

Generate a new `offload.toml` configuration file.

| Flag | Description |
|------|-------------|
| `-p, --provider TYPE` | Provider type: `local`, `default` (default: `local`) |
| `-f, --framework TYPE` | Framework type: `pytest`, `cargo`, `default` (default: `pytest`) |

### `offload logs`

View per-test results from the most recent run. Reads the JUnit XML report
at `{output_dir}/{junit_file}` (default: `test-results/junit.xml`).

| Flag | Description |
|------|-------------|
| `--failures` | Show only failed tests |
| `--errors` | Show only errored tests |
| `--test ID` | Show only the test with this exact ID (repeatable) |
| `--test-regex PATTERN` | Show only tests whose ID matches this regex (substring match) |

All flags compose with AND logic. For example, `offload logs --failures --test-regex "test_math"`
shows only failed tests whose ID contains `test_math`.

With no flags, all test results are printed. Each test is separated by a banner:

```
=== tests/test_math.py::test_add [PASSED] ===

=== tests/test_math.py::test_div [FAILED] ===
AssertionError: expected 2 got 3
tests/test_math.py:10: in test_div
    assert 1 / 0 == 2
E   AssertionError: expected 2 got 3
```

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All tests passed |
| 1 | Test failures or tests not run |
| 2 | Flaky tests only (passed on retry) |

## Configuration Reference

Configuration is stored in a TOML file (default: `offload.toml`).

### `[offload]` -- Core Settings

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_parallel` | integer | `10` | Maximum number of parallel sandboxes |
| `test_timeout_secs` | integer | `900` | Timeout per test batch in seconds |
| `working_dir` | string | (cwd) | Working directory for test execution |
| `sandbox_project_root` | string | required | Project root path on the remote sandbox (exported as `OFFLOAD_ROOT`) |
| `sandbox_init_cmd` | string | (none) | Optional command to run during image build, after cwd/copy-dirs are applied |

### `[provider]` -- Execution Provider

The `type` field selects the provider. One of: `local`, `default`, `modal`.

#### `type = "local"`

Run tests as local child processes.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `working_dir` | string | (cwd) | Working directory for spawned processes |
| `env` | table | `{}` | Environment variables for test processes |
| `shell` | string | `/bin/sh` | Shell used to execute commands |

#### `type = "default"`

Custom shell commands for sandbox lifecycle management. Commands use placeholder variables that are replaced via simple string substitution at runtime.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `prepare_command` | string | (none) | Runs once before sandbox creation. Must print an image ID as its last line of stdout (e.g. `im-rlXozWoN3Q9TWD8I6fnxm5`) |
| `create_command` | string | required | Creates a sandbox. Must print a sandbox ID to stdout (e.g. `sb-xyz123`). `{image_id}` is replaced with the output of `prepare_command` |
| `exec_command` | string | required | Runs a command inside a sandbox. `{sandbox_id}` is replaced with the sandbox ID from `create_command`. `{command}` is replaced with the full shell-escaped command string (program + args + env vars as a single quoted argument) |
| `destroy_command` | string | required | Destroys a sandbox. `{sandbox_id}` is replaced with the sandbox ID |
| `download_command` | string | (none) | Downloads files from a sandbox. `{sandbox_id}` is replaced with the sandbox ID. `{paths}` is replaced with space-separated `'remote':'local'` pairs |
| `working_dir` | string | (cwd) | Working directory for lifecycle commands |
| `timeout_secs` | integer | `3600` | Timeout for remote commands in seconds |
| `copy_dirs` | list | `[]` | Directories to copy into the image (`"local:remote"` format) |
| `env` | table | `{}` | Environment variables for test processes |

#### `type = "modal"`

Simplified Modal sandbox provider. Internally generates the appropriate Modal CLI commands.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `dockerfile` | string | (none) | Path to Dockerfile for building the sandbox image |
| `include_cwd` | boolean | `false` | Copy the current working directory into the image |
| `copy_dirs` | list | `[]` | Directories to copy into the image (`"local:remote"` format) |

### `[framework]` -- Test Framework

The `type` field selects the framework. One of: `pytest`, `cargo`, `default`.

#### `type = "pytest"`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `paths` | list | `["tests"]` | Directories to search for tests |
| `extra_args` | list | `[]` | Additional pytest arguments for discovery |
| `python` | string | `"python"` | Python interpreter to use |

#### `type = "cargo"`

No additional configuration. Requires [cargo-nextest](https://nexte.st/).

#### `type = "default"`

Custom shell commands for test discovery and execution.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `discover_command` | string | required | Command that outputs one test ID per line to stdout. Must contain `{filters}` placeholder |
| `run_command` | string | required | Command template; `{tests}` is replaced with space-separated test IDs. `{result_file}` is replaced with the result file path if configured |
| `result_file` | string | (none) | Path to JUnit XML result file produced by the test runner |
| `working_dir` | string | (cwd) | Working directory for test commands |
| `test_id_format` | string | required | Format for test IDs from JUnit XML (`{name}`, `{classname}`) |

### `[groups.NAME]` -- Test Groups

At least one group is required. Each group runs its own test discovery with its filters.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `retry_count` | integer | `0` | Number of times to retry failed tests |
| `filters` | string | `""` | Filter string passed to the framework during discovery. For pytest: pytest args (e.g. `-m 'not slow'`). For cargo: nextest list args. For default: substituted into `{filters}` placeholder in `discover_command` |

Failed tests that pass on retry are marked as "flaky" (exit code 2).

### `[report]` -- Reporting

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `output_dir` | string | `"test-results"` | Directory for report files |
| `junit` | boolean | `true` | Enable JUnit XML output |
| `junit_file` | string | `"junit.xml"` | Filename for JUnit XML output |

## Example Configurations

Example configuration files are included in the repository root.

### Local Cargo Tests (`offload.toml`)

```toml
[offload]
max_parallel = 4
test_timeout_secs = 300
sandbox_project_root = "."

[provider]
type = "local"
working_dir = "."

[framework]
type = "cargo"

[groups.all]
retry_count = 0

[report]
output_dir = "test-results"
```

### Pytest on Modal (`offload-pytest-default.toml`)

```toml
[offload]
max_parallel = 4
test_timeout_secs = 600
sandbox_project_root = "/app"

[provider]
type = "default"
prepare_command = "uv run @modal_sandbox.py prepare --include-cwd examples/Dockerfile"
create_command = "uv run @modal_sandbox.py create {image_id}"
exec_command = "uv run @modal_sandbox.py exec {sandbox_id} {command}"
destroy_command = "uv run @modal_sandbox.py destroy {sandbox_id}"
download_command = "uv run @modal_sandbox.py download {sandbox_id} {paths}"
timeout_secs = 600

[framework]
type = "pytest"
paths = ["examples/tests"]
python = "uv"
extra_args = ["run", "--with=pytest"]

[groups.unit]
retry_count = 2
filters = "-m 'not slow'"

[groups.slow]
retry_count = 3
filters = "-m 'slow'"

[report]
output_dir = "test-results"
```

### Cargo Tests on Modal (`offload-cargo-modal.toml`)

```toml
[offload]
max_parallel = 4
test_timeout_secs = 600
sandbox_project_root = "/app"

[provider]
type = "modal"
dockerfile = ".devcontainer/Dockerfile"
include_cwd = true

[framework]
type = "cargo"

[groups.all]
retry_count = 1

[report]
output_dir = "test-results"
```

### Default Framework on Modal (`offload-pytest-default.toml` from mng)

```toml
[offload]
max_parallel = 40
test_timeout_secs = 60
sandbox_project_root = "/code/mng"
sandbox_init_cmd = "git apply /offload-upload/patch --allow-empty && uv sync --all-packages"

[provider]
type = "default"
prepare_command = "uv run @modal_sandbox.py prepare --include-cwd --cached libs/mng/imbue/mng/resources/Dockerfile"
create_command = "uv run @modal_sandbox.py create {image_id}"
exec_command = "uv run @modal_sandbox.py exec {sandbox_id} {command}"
destroy_command = "uv run @modal_sandbox.py destroy {sandbox_id}"
download_command = "uv run @modal_sandbox.py download {sandbox_id} {paths}"
timeout_secs = 600

[framework]
type = "pytest"
paths = ["libs/mng/tests"]
python = "uv"
extra_args = ["run", "--with=pytest"]

[groups.all]
retry_count = 0
filters = "-m 'not acceptance and not release'"

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

This demonstrates using `sandbox_init_cmd` to run setup commands during image build. The `sandbox_init_cmd` applies a patch and syncs packages after the working directory is copied into the image, enabling the use of the native `pytest` framework instead of the `default` framework with inline setup commands.

## Bundled Scripts

Commands in configuration can reference bundled scripts using `@filename.ext` syntax. For example, `uv run @modal_sandbox.py create {image_id}` references the bundled `modal_sandbox.py` script. Scripts are extracted to a cache directory on first use.

## Image Cache

When using the `modal` provider or a `default` provider with a `prepare_command`, the bundled `modal_sandbox.py` script caches the image ID in `.offload-image-cache` at the project root. Delete this file to force a fresh image build on the next run. You can also pass `--no-cache` to `offload run` to skip cached image lookup.

## Environment Variable Expansion

Configuration values support environment variable expansion:

- `${VAR}` -- required; fails if `VAR` is not set
- `${VAR:-default}` -- uses `default` if `VAR` is not set

## Self-Testing

Offload can run its own test suite on Modal:

```bash
cargo run -- -c offload-pytest-default.toml run
```

This requires a valid Modal API key.

## License

All Rights Reserved. See [LICENSE](LICENSE) for details.
