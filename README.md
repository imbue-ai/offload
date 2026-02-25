# offload

A flexible parallel test runner written in Rust with pluggable execution providers.

## Features

- **Parallel execution** across multiple sandboxes
- **Automatic retry** for flaky tests
- **Multiple providers**: local processes or plugin scripts to invoke ephemeral compute
- **Test discovery**: pytest, cargo test, or custom commands
- **JUnit XML** reporting

## Installation

```bash
cargo install --path .
```

## Quick Start

1. Initialize a config file:
```bash
offload init --provider process --framework pytest
```

2. Run tests:
```bash
offload run
```

## Configuration

Create a `offload.toml` file in your project root.

### Core Settings

```toml
[offload]
max_parallel = 4          # Number of parallel sandboxes
test_timeout_secs = 300   # Timeout per test

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

## Test Discovery

### pytest

```toml
[framework]
type = "pytest"
paths = ["tests"]
python = "python3"
markers = "not slow"  # Optional: filter by markers

[groups.unit]
retry_count = 2       # Optional: retries for failed tests (default: 0)
```

### Cargo Test

```toml
[framework]
type = "cargo"
package = "my-crate"  # Optional: for workspaces
features = ["feature1", "feature2"]
include_ignored = false

[groups.rust]
retry_count = 0
```

### Generic (Custom)

```toml
[framework]
type = "default"
test_id_format = "{name}"  # Required for default framework
discover_command = "find tests -name 'test_*.py' | xargs -I {} basename {}"
run_command = "pytest {tests} -v"

[groups.custom]
retry_count = 0
```

The `{tests}` placeholder is replaced with discovered test names.

## CLI Commands

```bash
# Run all tests
offload run

# Run with more parallelism
offload run --parallel 8

# Discover tests without running
offload collect

# Validate configuration
offload validate

# Initialize new config
offload init --provider ssh --framework pytest
```

## Example Configurations

Example configurations have been provided in the root of this repo. See offload-*.toml for examples.


### Testing

Use the project to test itself with:

```
cargo run -- -c offload-modal.toml run
```

(Requires valid Modal API key)

## License

All Rights Reserved. See [LICENSE](LICENSE) for details.
