# offload

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
cargo install offload@0.3.3
```

From source:

```bash
cargo install --path .
```

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
| `stream_output` | boolean | `false` | Stream test output in real-time |
| `sandbox_project_root` | string | required | Project root path on the remote sandbox (exported as `OFFLOAD_ROOT`) |

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

Custom shell commands for sandbox lifecycle management. Supports placeholders: `{sandbox_id}`, `{command}`, `{image_id}`, `{paths}`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `prepare_command` | string | (none) | Runs once before sandbox creation; prints image ID to stdout |
| `create_command` | string | required | Creates a sandbox; prints sandbox ID to stdout. Can use `{image_id}` |
| `exec_command` | string | required | Runs a command on a sandbox. Uses `{sandbox_id}` and `{command}` |
| `destroy_command` | string | required | Destroys a sandbox. Uses `{sandbox_id}` |
| `download_command` | string | (none) | Downloads files from sandbox. Uses `{sandbox_id}` and `{paths}` |
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
| `markers` | string | (none) | pytest marker expression to filter tests |
| `extra_args` | list | `[]` | Additional pytest arguments for discovery |
| `python` | string | `"python"` | Python interpreter to use |
| `test_id_format` | string | `"{name}"` | Format for test IDs from JUnit XML (`{name}`, `{classname}`) |

#### `type = "cargo"`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `package` | string | (none) | Package to test in a Cargo workspace |
| `features` | list | `[]` | Cargo features to enable |
| `bin` | string | (none) | Specific binary to test |
| `include_ignored` | boolean | `false` | Include `#[ignore]` tests |
| `test_id_format` | string | `"{classname} {name}"` | Format for test IDs from JUnit XML (`{name}`, `{classname}`) |

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
stream_output = true
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

### Pytest on Modal (`offload-modal.toml`)

```toml
[offload]
max_parallel = 4
test_timeout_secs = 600
stream_output = true
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
stream_output = true
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

### Default Framework on Modal (`offload-modal.toml` from mng)

```toml
[offload]
max_parallel = 40
test_timeout_secs = 60
stream_output = true
sandbox_project_root = "/code/mng"

[provider]
type = "default"
prepare_command = "uv run @modal_sandbox.py prepare --cached libs/mng/imbue/mng/resources/Dockerfile"
create_command = "uv run @modal_sandbox.py create {image_id}"
exec_command = "uv run @modal_sandbox.py exec {sandbox_id} {command}"
destroy_command = "uv run @modal_sandbox.py destroy {sandbox_id}"
download_command = "uv run @modal_sandbox.py download {sandbox_id} {paths}"
timeout_secs = 600

[framework]
type = "default"
discover_command = "uv sync --all-packages && uv run pytest --collect-only -q {filters} 2>/dev/null | grep '::'"
run_command = "cd /code/mng && uv sync --all-packages && uv run pytest -v --tb=short --no-cov -p no:xdist -o addopts= --junitxml={result_file} {tests}"
test_id_format = "{name}"

[groups.all]
retry_count = 0
filters = "-m 'not acceptance and not release'"

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

This demonstrates using the `default` framework with custom pytest discovery and execution on Modal. This is necessary when the built-in `pytest` framework doesn't support your workflow — common reasons include monorepo workspaces requiring pre-sync steps (`uv sync --all-packages`), conflicting `addopts` in `pyproject.toml` (e.g. xdist workers or coverage that must be disabled), or pre-test setup commands. Better support for these workflows in the built-in frameworks is planned for upcoming versions.

## Bundled Scripts

Commands in configuration can reference bundled scripts using `@filename.ext` syntax. For example, `uv run @modal_sandbox.py create {image_id}` references the bundled `modal_sandbox.py` script. Scripts are extracted to a cache directory on first use.

## Image Cache

When using the `modal` provider or a `default` provider with a `prepare_command`, the bundled `modal_sandbox.py` script caches the image ID in `.offload-image-cache` at the project root. Delete this file to force a fresh image build on the next run. You can also pass `--no-cache` to `offload run` to skip cached image lookup.

## Environment Variable Expansion

Configuration values support environment variable expansion:

- `${VAR}` -- required; fails if `VAR` is not set
- `${VAR:-default}` -- uses `default` if `VAR` is not set

## Self-Testing

offload can run its own test suite on Modal:

```bash
cargo run -- -c offload-modal.toml run
```

This requires a valid Modal API key.

## License

All Rights Reserved. See [LICENSE](LICENSE) for details.
