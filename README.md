# shotgun

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
shotgun init --provider process --framework pytest
```

2. Run tests:
```bash
shotgun run
```

## Configuration

Create a `shotgun.toml` file in your project root.

### Core Settings

```toml
[shotgun]
max_parallel = 4          # Number of parallel sandboxes
test_timeout_secs = 300   # Timeout per test
retry_count = 2           # Retries for failed tests

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

## Test Discovery

### pytest

```toml
[discovery]
type = "pytest"
paths = ["tests"]
python = "python3"
markers = "not slow"  # Optional: filter by markers
```

### Cargo Test

```toml
[discovery]
type = "cargo"
package = "my-crate"  # Optional: for workspaces
features = ["feature1", "feature2"]
include_ignored = false
```

### Generic (Custom)

```toml
[discovery]
type = "generic"
discover_command = "find tests -name 'test_*.py' | xargs -I {} basename {}"
run_command = "pytest {tests} -v"
```

The `{tests}` placeholder is replaced with discovered test names.

## CLI Commands

```bash
# Run all tests
shotgun run

# Run with more parallelism
shotgun run --parallel 8

# Discover tests without running
shotgun collect

# Validate configuration
shotgun validate

# Initialize new config
shotgun init --provider ssh --framework pytest
```

## Example Configurations

Example configurations have been provided in the root of this repo. See shotgut-*.toml for examples.


### Testing

Use the project to test itself with:

```
cargo run -- -c shotgun-modal.toml run
```

(Requires valid Modal API key)

## License

MIT
