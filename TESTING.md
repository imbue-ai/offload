# Testing

This document describes how to test offload itself.

## Running Tests

Offload uses cargo nextest for testing:

```bash
cargo nextest run
```

Or with standard cargo test:

```bash
cargo test
```

## Test Organization

Tests are organized as:

- **Unit tests**: Inline `#[cfg(test)]` modules in source files
- **Integration tests**: Would be in `tests/` directory

Key test locations:

| File | Tests |
|------|-------|
| `src/orchestrator/scheduler.rs` | Scheduling algorithms |
| `src/connector.rs` | Shell execution and streaming |
| `src/config.rs` | Environment variable expansion |
| `src/provider/default.rs` | Command template building |
| `src/config/schema.rs` | Configuration parsing |

## Linting and Formatting

Before committing, ensure code passes:

```bash
# Format check
cargo fmt --check

# Clippy lints (no warnings allowed)
cargo clippy

# Run tests
cargo nextest run
```

## Test Patterns

### Testing Async Code

Use `#[tokio::test]` for async tests:

```rust
#[tokio::test]
async fn test_run_stream_yields_exit_code_success() {
    let connector = ShellConnector::new();
    let mut stream = connector.run_stream("echo hello").await.unwrap();
    // ...
}
```

### Testing Configuration

Parse inline TOML strings:

```rust
#[test]
fn test_modal_provider_with_dockerfile() -> Result<(), Box<dyn std::error::Error>> {
    let toml = r#"
        [offload]
        max_parallel = 4

        [provider]
        type = "modal"
        app_name = "offload-sandbox"
        dockerfile = ".devcontainer/Dockerfile"

        [groups.test]
        type = "pytest"
    "#;

    let config: Config = toml::from_str(toml)?;
    // assertions...
    Ok(())
}
```

### Testing with Environment Variables

Use predictable environment variables in tests:

```rust
#[test]
fn test_expand_env_value_var_set() -> Result<(), String> {
    // HOME is always set in Unix environments
    let result = expand_env_value("${HOME}")?;
    assert!(!result.is_empty());
    Ok(())
}

#[test]
fn test_expand_env_value_var_unset() {
    // Use a guaranteed non-existent variable
    let result = expand_env_value("${_OFFLOAD_TEST_NONEXISTENT_VAR}");
    assert!(result.is_err());
}
```

## Definition of Done

For any code change to be considered complete:

1. `cargo fmt --check` passes
2. `cargo clippy` passes with no warnings
3. `cargo nextest run` passes
