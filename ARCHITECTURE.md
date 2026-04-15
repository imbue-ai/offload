# Architecture

## Design Principles

### Rust owns all logic; Python is a thin SDK wrapper

`scripts/modal_sandbox.py` exists solely because the Modal SDK is Python-only. It is a **thin, stateless wrapper** that translates CLI arguments into Modal API calls and returns results (image IDs, sandbox IDs) to stdout.

All caching, fallback, retry, and decision-making logic lives in Rust. Python must not accrete functionality beyond what is strictly required to call the Modal SDK. If a feature can be implemented in Rust, it must be.

### Provider architecture

The `SandboxProvider` trait (`src/provider.rs`) defines the interface for execution environments. Each provider manages the full sandbox lifecycle: prepare, create, execute, download, destroy.

Three providers are implemented:

| Provider | Config type | Description |
|----------|-----------|-------------|
| **Local** | `type = "local"` | Runs tests as child processes on the host machine. No containerization. |
| **Default** | `type = "default"` | Shell-command-based lifecycle. The user supplies `create_command`, `exec_command`, `destroy_command` (and optionally `prepare_command`, `download_command`). Used to integrate with any cloud sandbox backend. |
| **Modal** | `type = "modal"` | Simplified configuration for Modal sandboxes. Generates `default`-style commands internally using the bundled `modal_sandbox.py` script. |

The Modal provider is a convenience layer over the Default provider — it produces the same `DefaultSandbox` at runtime. The Default provider is the general-purpose escape hatch: any execution backend that exposes a CLI can be integrated through its command templates.

All providers support automatic retry of transient failures (timeouts, connection errors) via the `with_retry!` macro (`src/provider/retry.rs`), which wraps provider operations with exponential backoff.

### Framework architecture

The `TestFramework` trait (`src/framework.rs`) defines how tests are discovered and executed. Each framework handles discovery (collecting test IDs) and command generation (producing the shell command to run a batch of tests).

Four frameworks are implemented:

| Framework | Config type | Discovery method |
|-----------|-----------|------------------|
| **pytest** | `type = "pytest"` | `{command} --collect-only -q` |
| **nextest** | `type = "nextest"` | `cargo nextest list --message-format json` |
| **vitest** | `type = "vitest"` | `{command} list --json` |
| **default** | `type = "default"` | User-supplied `discover_command` |

All frameworks produce results via JUnit XML (or convert to it, as vitest does from JSON). The `test_id_format` field controls how JUnit XML attributes are mapped back to discovered test IDs.

### Execution flow

A test run proceeds through these phases:

1. **Config loading** — Parse `offload.toml`, expand environment variables, validate.
2. **Discovery** — Each group runs its framework's discovery command locally, producing a list of test IDs.
3. **Scheduling** — Tests are batched across sandboxes. LPT (Longest Processing Time) scheduling is used when historical timing data is available; otherwise round-robin.
4. **Preparation** — If the provider has a `prepare_command` (or equivalent), build/cache the sandbox image once.
5. **Execution** — Sandboxes are created in a pool (up to `max_parallel`). Each sandbox runs one batch, downloads results, and is destroyed.
6. **Result aggregation** — JUnit XML results are parsed, matched to discovered test IDs via `test_id_format`, and merged into a final report.
7. **Retry** — Failed tests are re-queued for retry (up to `retry_count` per group). Tests that pass on retry are marked flaky.

## Versioning

A change is **breaking** if it would cause a previously correct `offload run` invocation — same CLI flags, same `offload.toml`, same test suite — to be rejected or to produce a different exit code. Everything else (new optional fields, new warnings, internal refactors, message changes) is not breaking.
