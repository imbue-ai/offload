# Guide for AI Agents

Instructions for AI agents working on this codebase.

## Project Overview

Offload is a parallel test runner written in Rust. It executes tests across isolated sandboxes using pluggable providers (local processes, Modal cloud, or custom shell commands) and frameworks (pytest, cargo nextest, or custom).

## Key Files

| File | Purpose |
|------|---------|
| `src/main.rs` | CLI entry point, command parsing |
| `src/lib.rs` | Library root, public API |
| `src/config.rs` | Configuration loading |
| `src/config/schema.rs` | All configuration types |
| `src/provider.rs` | `SandboxProvider` and `Sandbox` traits |
| `src/framework.rs` | `TestFramework` trait and test types |
| `src/orchestrator.rs` | Test execution coordination |
| `src/orchestrator/runner.rs` | `TestRunner` for sandbox execution |
| `src/orchestrator/scheduler.rs` | Test distribution algorithms |

## Required Checks

Before any commit, ensure:

```bash
cargo fmt --check    # Formatting
cargo clippy         # No warnings
cargo nextest run    # Tests pass
```

## Code Style

- Use Rust 2024 edition features
- Prefer `anyhow::Result` for error handling in binaries
- Use `thiserror` for library error types
- Document public APIs with doc comments
- Use `tracing` for logging (debug, info, warn, error)

## Common Patterns

### Adding a New Provider

1. Create `src/provider/myprovider.rs`
2. Implement `Sandbox` trait for your sandbox type
3. Implement `SandboxProvider` trait for your provider type
4. Add config type to `src/config/schema.rs` (`ProviderConfig` enum)
5. Wire up in `src/main.rs` match statements

### Adding a New Framework

1. Create `src/framework/myframework.rs`
2. Implement `TestFramework` trait
3. Add config type to `src/config/schema.rs` (`FrameworkConfig` enum)
4. Wire up in `src/main.rs` match statements

### Configuration Pattern

All config uses serde with TOML:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MyConfig {
    VariantA(VariantAConfig),
    VariantB(VariantBConfig),
}
```

### Async Patterns

- Use `tokio` runtime with `multi_thread` flavor
- Use `tokio_scoped::scope` for parallel tasks that borrow
- Use `CancellationToken` for graceful shutdown
- Use `Arc<Mutex<T>>` for shared mutable state

## Architecture Summary

```
CLI → Config → Framework.discover() → tests
                                        │
                                        ▼
                               Scheduler.schedule()
                                        │
                                        ▼
                               parallel batches
                                        │
                    ┌───────────────────┼───────────────────┐
                    ▼                   ▼                   ▼
              TestRunner          TestRunner          TestRunner
                  │                   │                   │
                  ▼                   ▼                   ▼
               Sandbox             Sandbox             Sandbox
              (Provider)          (Provider)          (Provider)
                  │                   │                   │
                  └───────────────────┼───────────────────┘
                                      ▼
                             JUnit XML collection
                                      │
                                      ▼
                             Report + Summary
```
