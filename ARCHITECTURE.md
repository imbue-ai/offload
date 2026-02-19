# Architecture

This document describes the internal architecture of offload.

## Module Structure

```
src/
├── main.rs           # CLI entry point and command handling
├── lib.rs            # Library root with public API
├── config.rs         # Configuration loading
├── config/
│   └── schema.rs     # Configuration type definitions
├── provider.rs       # Provider traits (SandboxProvider, Sandbox)
├── provider/
│   ├── local.rs      # Local process provider
│   ├── modal.rs      # Modal cloud provider
│   └── default.rs    # Custom shell command provider
├── framework.rs      # Framework traits (TestFramework)
├── framework/
│   ├── pytest.rs     # pytest support
│   ├── cargo.rs      # cargo nextest support
│   └── default.rs    # Custom shell command framework
├── orchestrator.rs   # Test execution coordination
├── orchestrator/
│   ├── runner.rs     # TestRunner for single sandbox
│   ├── scheduler.rs  # Test distribution algorithms
│   └── pool.rs       # SandboxPool management
├── connector.rs      # Shell command execution abstraction
├── report.rs         # Result reporting
├── report/
│   └── junit.rs      # JUnit XML generation
├── cache.rs          # Image caching for Modal provider
└── bundled.rs        # Bundled Python scripts for Modal
```

## Core Abstractions

### Provider System

The provider system creates isolated execution environments (sandboxes).

```
SandboxProvider (trait)
├── create_sandbox(config) → Sandbox
└── base_env() → Vec<(String, String)>

Sandbox (trait)
├── id() → &str
├── exec_stream(Command) → OutputStream
├── upload(local, remote)
├── download(paths)
└── terminate()
```

**Implementations:**

| Provider | Description | Use Case |
|----------|-------------|----------|
| `LocalProvider` | Child processes | Development, simple CI |
| `ModalProvider` | Modal cloud sandboxes | Cloud execution with caching |
| `DefaultProvider` | Shell command templates | Any cloud provider |

### Framework System

Frameworks discover tests and parse results.

```
TestFramework (trait)
├── discover(paths) → Vec<TestRecord>
├── produce_test_execution_command(tests) → Command
└── parse_results(output, result_file) → Vec<TestResult>
```

**Implementations:**

| Framework | Discovery | Result Parsing |
|-----------|-----------|----------------|
| `PytestFramework` | `pytest --collect-only -q` | JUnit XML or stdout |
| `CargoFramework` | `cargo nextest list` | JUnit XML |
| `DefaultFramework` | Custom shell command | JUnit XML or exit code |

### Test Records and Instances

```
TestRecord
├── id: String              # Unique test identifier
├── name: String            # Display name
├── file: Option<PathBuf>   # Source file
├── markers: Vec<String>    # Tags/labels
├── retry_count: usize      # Per-test retry count
├── group: Option<String>   # Group name
└── results: Mutex<Vec<TestResult>>  # Interior mutability for results

TestInstance<'a>           # Lightweight handle for execution
└── record: &'a TestRecord
```

### Orchestrator

The orchestrator coordinates the entire test run:

1. **Test Discovery**: Uses framework to find tests
2. **Instance Expansion**: Creates retry instances (N = retry_count + 1)
3. **Scheduling**: Distributes instances across sandboxes
4. **Parallel Execution**: Runs batches concurrently via tokio-scoped
5. **Result Collection**: Aggregates results from JUnit XML
6. **Early Stopping**: Cancels remaining work when all tests pass

```
Orchestrator<S, D>
├── config: Config
├── framework: D (TestFramework)
└── verbose: bool

run_with_tests(tests, sandbox_pool) → RunResult
```

### Scheduler

Distributes tests across parallel sandboxes.

```
Scheduler
├── schedule(tests) → Vec<Vec<TestInstance>>        # Round-robin
├── schedule_random(tests) → ...                     # Shuffled round-robin
├── schedule_with_batch_size(tests, size) → ...      # Fixed batch size
└── schedule_individual(tests) → ...                 # One test per batch
```

### TestRunner

Executes tests within a single sandbox.

```
TestRunner<S, D>
├── sandbox: S
├── framework: &D
├── timeout: Duration
├── output_callback: Option<OutputCallback>
├── cancellation_token: Option<CancellationToken>
└── junit_report: Option<SharedJunitReport>

run_tests(tests) → Result<bool>
```

Features:
- Streaming output with optional callback
- Cancellation support for early stopping
- JUnit XML download and parsing

## Execution Flow

```
1. CLI parses arguments
   │
2. Load configuration (offload.toml)
   │
3. For each group, discover tests
   │  └─ Framework.discover() → Vec<TestRecord>
   │
4. Expand tests with retry count
   │  └─ N instances per test (retry_count + 1)
   │
5. Create sandbox pool
   │  └─ Provider.create_sandbox() × max_parallel
   │
6. Schedule tests into batches
   │  └─ Scheduler.schedule() → Vec<Vec<TestInstance>>
   │
7. Execute batches in parallel (tokio-scoped)
   │  ├─ TestRunner.run_tests(batch)
   │  │   ├─ Framework.produce_test_execution_command()
   │  │   ├─ Sandbox.exec_stream(command)
   │  │   ├─ Sandbox.download(/tmp/junit.xml)
   │  │   └─ Add results to shared JUnit report
   │  │
   │  └─ Early stop if all tests pass
   │
8. Aggregate results
   │  └─ JunitReport.summary() → (passed, failed, flaky)
   │
9. Write JUnit XML and print summary
   │
10. Terminate all sandboxes
```

## Concurrency Model

Offload uses `tokio-scoped` for parallel execution, which allows spawning tasks that borrow from the parent scope. This avoids the `'static` requirement of regular `tokio::spawn`.

```rust
tokio_scoped::scope(|scope| {
    for (sandbox, batch) in sandboxes.zip(batches) {
        scope.spawn(async move {
            // Can borrow from parent scope
            runner.run_tests(&batch).await
        });
    }
});
```

Key synchronization primitives:
- `Arc<Mutex<MasterJunitReport>>` - Shared report accumulator
- `AtomicBool` - Early stopping flag
- `CancellationToken` - Graceful task cancellation

## Result Aggregation

Results flow through the following path:

1. Test runs produce JUnit XML at `/tmp/junit.xml` in sandbox
2. `TestRunner` downloads XML via `Sandbox.download()`
3. XML content added to `MasterJunitReport`
4. Report parses XML and tracks per-test results
5. Flaky detection: test passed after initial failure
6. Final XML written to configured output directory

## Provider Protocol (Default Provider)

The default provider uses shell commands with placeholders:

```
prepare_command → image_id (stdout, last line)
                      │
create_command ───────┴─── {image_id} → sandbox_id (stdout)
                               │
exec_command ──────────────────┴─── {sandbox_id}, {command} → output
                               │
download_command ──────────────┴─── {sandbox_id}, {paths} → files
                               │
destroy_command ───────────────┴─── {sandbox_id} → cleanup
```

## Connector

The `Connector` trait provides low-level shell command execution:

```
Connector (trait)
├── run(command) → ExecResult       # Buffered execution
└── run_stream(command) → OutputStream  # Streaming

ShellConnector
├── working_dir: Option<PathBuf>
└── timeout_secs: u64
```

Used internally by `DefaultProvider` and `ModalProvider` to execute lifecycle commands.

## Bundled Scripts

Modal provider uses bundled Python scripts for sandbox management:

- `@modal_sandbox.py` - Modal sandbox lifecycle (prepare, create, exec, destroy, download)

The `@` prefix triggers expansion to the bundled script's extracted path.

## Image Caching

`ModalProvider` caches built images to avoid redundant builds:

- Cache key: `dockerfile:{path}`
- Validation: SHA-256 hash of Dockerfile content
- Storage: `.offload-cache.json` in working directory
- Thread-safe: Uses `OnceCell` for concurrent build deduplication
