# offload

A flexible parallel test runner with pluggable providers for running tests across local processes or cloud environments.

## Overview

Offload enables distributed test execution by:

- Running tests in parallel across multiple isolated sandboxes
- Supporting automatic test discovery for pytest, cargo test, and custom frameworks
- Detecting flaky tests through configurable retry logic
- Generating JUnit XML reports for CI/CD integration
- Streaming real-time test output

## Installation

```bash
cargo install offload
```

Or build from source:

```bash
cargo build --release
```

## Quick Start

1. Create an `offload.toml` configuration file:

```bash
offload init --provider local --framework pytest
```

2. Run your tests:

```bash
offload run
```

## CLI Commands

```
offload run [OPTIONS]        Run tests
offload collect              Discover tests without running them
offload validate             Validate configuration file
offload init                 Initialize a new configuration file
```

### Run Options

- `-c, --config <PATH>` - Configuration file path (default: `offload.toml`)
- `-p, --parallel <N>` - Override maximum parallel sandboxes
- `--collect-only` - Only discover tests, don't run them
- `--copy-dir <LOCAL:REMOTE>` - Directories to copy to sandbox
- `--env <KEY=VALUE>` - Environment variables to set in sandboxes
- `-v, --verbose` - Enable verbose output with streaming test output

## Configuration

Offload is configured via TOML files. The configuration has four main sections:

### Core Settings (`[offload]`)

```toml
[offload]
max_parallel = 10           # Number of parallel sandboxes
test_timeout_secs = 900     # Per-batch timeout (15 min default)
retry_count = 3             # Retries for failed tests
working_dir = "."           # Working directory for tests
stream_output = false       # Stream output in real-time
```

### Provider Configuration (`[provider]`)

Providers determine where tests execute.

#### Local Provider

Runs tests as local child processes:

```toml
[provider]
type = "local"
working_dir = "/path/to/project"
shell = "/bin/bash"

[provider.env]
PYTHONPATH = "/app"
```

#### Modal Provider

Runs tests in Modal cloud sandboxes with Dockerfile-based images:

```toml
[provider]
type = "modal"
app_name = "offload-sandbox"
dockerfile = ".devcontainer/Dockerfile"
timeout_secs = 600

[provider.env]
API_KEY = "${API_KEY}"
```

#### Default Provider

Runs tests using custom shell commands for any cloud/execution environment:

```toml
[provider]
type = "default"
prepare_command = "./scripts/build-image.sh"      # Optional: returns image_id
create_command = "./scripts/create.sh {image_id}" # Returns sandbox_id
exec_command = "./scripts/exec.sh {sandbox_id} {command}"
destroy_command = "./scripts/destroy.sh {sandbox_id}"
download_command = "./scripts/download.sh {sandbox_id} {paths}"
timeout_secs = 3600
copy_dirs = ["./src:/app/src", "./tests:/app/tests"]

[provider.env]
MY_VAR = "value"
```

### Test Groups (`[groups.<name>]`)

Groups organize tests by framework. All groups in a configuration must use the same framework type.

#### pytest

```toml
[groups.python]
type = "pytest"
paths = ["tests"]
markers = "not slow"
python = "python3"
extra_args = ["-x"]
```

#### cargo (via nextest)

```toml
[groups.rust]
type = "cargo"
package = "my-crate"
features = ["test-utils"]
include_ignored = false
```

#### Custom Framework

```toml
[groups.custom]
type = "default"
discover_command = "jest --listTests --json | jq -r '.[]'"
run_command = "jest {tests} --ci --reporters=jest-junit"
result_file = "junit.xml"
working_dir = "."
```

### Report Configuration (`[report]`)

```toml
[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
```

## Environment Variable Expansion

Provider environment variables support shell-style expansion:

- `${VAR}` - Required variable (fails if not set)
- `${VAR:-default}` - Optional with default value
- `$$` - Escaped dollar sign

```toml
[provider.env]
API_KEY = "${API_KEY}"
DEBUG = "${DEBUG:-false}"
PRICE = "$$100"
```

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All tests passed |
| 1 | Some tests failed or weren't run |
| 2 | All tests passed but some were flaky |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Orchestrator                             │
│   Coordinates test discovery, scheduling, and result collection  │
└─────────────────────────────────────────────────────────────────┘
              │                                │
              ▼                                ▼
┌─────────────────────────┐      ┌─────────────────────────┐
│       Framework          │      │        Provider          │
│  - pytest                │      │  - local (processes)     │
│  - cargo (nextest)       │      │  - modal (cloud)         │
│  - default (custom)      │      │  - default (shell cmds)  │
│                          │      │                          │
│  discover() → tests      │      │  create_sandbox()        │
│  run_command() → cmd     │      │  exec_stream()           │
│  parse_results()         │      │  download()              │
└─────────────────────────┘      │  terminate()             │
                                  └─────────────────────────┘
              │                                │
              └────────────┬───────────────────┘
                           ▼
                  ┌────────────────┐
                  │   Scheduler     │
                  │  Distributes    │
                  │  tests across   │
                  │  sandboxes      │
                  └────────────────┘
```

## Library Usage

Offload can also be used as a library:

```rust
use offload::config::{load_config, SandboxConfig};
use offload::orchestrator::{Orchestrator, SandboxPool};
use offload::provider::local::LocalProvider;
use offload::framework::{TestFramework, pytest::PytestFramework};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = load_config(std::path::Path::new("offload.toml"))?;

    let provider = LocalProvider::new(Default::default());
    let framework = PytestFramework::new(Default::default());

    // Discover tests
    let tests = framework.discover(&[]).await?;

    // Create sandbox pool
    let sandbox_config = SandboxConfig {
        id: "sandbox".to_string(),
        working_dir: None,
        env: vec![],
        copy_dirs: vec![],
    };
    let mut sandbox_pool = SandboxPool::new();
    sandbox_pool.populate(config.offload.max_parallel, &provider, &sandbox_config).await?;

    // Run tests
    let orchestrator = Orchestrator::new(config, framework, false);
    let result = orchestrator.run_with_tests(&tests, sandbox_pool).await?;

    std::process::exit(result.exit_code());
}
```

## License

See LICENSE file.
