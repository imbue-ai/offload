# shotgun

A flexible parallel test runner written in Rust with pluggable execution providers.

## Features

- **Parallel execution** across multiple sandboxes
- **Automatic retry** for flaky tests
- **Multiple providers**: local processes, Docker, SSH, or custom scripts
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

## Providers

### Process Provider

Runs tests as local processes. Best for local development.

```toml
[provider]
type = "process"
working_dir = "."
shell = "/bin/sh"

[provider.env]
PYTHONPATH = "./src"
```

### Docker Provider

Runs tests in Docker containers. Good for isolated, reproducible environments.

```toml
[provider]
type = "docker"
image = "python:3.11"
working_dir = "/workspace"
network_mode = "bridge"
# Volume mounts in Docker bind format: "host:container"
volumes = [
    "./src:/workspace/src",
    "./tests:/workspace/tests",
]

[provider.env]
PYTHONPATH = "/workspace/src"

[provider.resources]
cpu_limit = 2.0
memory_limit = 4294967296  # 4GB in bytes
```

### SSH Provider

Runs tests on remote machines via SSH. Recommended for **EC2**, **GCP**, or any SSH-accessible server.

```toml
[provider]
type = "ssh"
hosts = [
    "ec2-12-34-56-78.compute-1.amazonaws.com",
    "ec2-98-76-54-32.compute-1.amazonaws.com",
]
user = "ubuntu"
key_path = "~/.ssh/my-ec2-key.pem"
port = 22
working_dir = "/home/ubuntu/workspace"
disable_host_key_check = true  # Set false in production
```

**Setup for EC2:**
1. Launch EC2 instance(s) with your test environment
2. Ensure security group allows SSH (port 22)
3. Copy your code to the working directory on each instance
4. Configure the provider with instance hostnames

Tests are distributed round-robin across all hosts.

### Remote Provider

Delegates execution to your own script. Recommended for **Modal** or custom cloud setups.

```toml
[provider]
type = "remote"
execute_command = "./scripts/run-on-modal.py"
setup_command = "./scripts/sync-code.sh"      # Optional: runs once before tests
teardown_command = "./scripts/cleanup.sh"     # Optional: runs after all tests
timeout_secs = 600

[provider.env]
MODAL_TOKEN_ID = "..."
```

Your script receives the test command as an argument:
```bash
./scripts/run-on-modal.py "pytest tests/test_math.py::test_add -v"
```

**Example Modal script (`scripts/run-on-modal.py`):**
```python
#!/usr/bin/env python3
import sys
import modal

app = modal.App("shotgun-test")
image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")

@app.function(image=image, timeout=600)
def run_test(cmd: str):
    import subprocess
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    print(result.stdout)
    print(result.stderr, file=sys.stderr)
    return result.returncode

if __name__ == "__main__":
    test_cmd = sys.argv[1]
    with app.run():
        exit_code = run_test.remote(test_cmd)
    sys.exit(exit_code)
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

### Local pytest with Docker

```toml
[shotgun]
max_parallel = 4
test_timeout_secs = 300
retry_count = 2

[provider]
type = "docker"
image = "python:3.11"
working_dir = "/app"
volumes = [".:/app"]

[discovery]
type = "pytest"
paths = ["tests"]

[report]
output_dir = "test-results"
junit = true
```

### Distributed testing on EC2

```toml
[shotgun]
max_parallel = 8
test_timeout_secs = 600
retry_count = 3

[provider]
type = "ssh"
hosts = [
    "10.0.1.10",
    "10.0.1.11",
    "10.0.1.12",
    "10.0.1.13",
]
user = "ubuntu"
key_path = "~/.ssh/ec2-key.pem"
working_dir = "/home/ubuntu/project"

[discovery]
type = "pytest"
paths = ["tests"]

[report]
output_dir = "test-results"
junit = true
```

### Modal execution

```toml
[shotgun]
max_parallel = 10
test_timeout_secs = 600
retry_count = 2

[provider]
type = "remote"
execute_command = "./scripts/run-on-modal.py"

[discovery]
type = "pytest"
paths = ["tests"]

[report]
output_dir = "test-results"
junit = true
```

## License

MIT
