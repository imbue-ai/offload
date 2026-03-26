# Spec: Provider-level retry for transient failures

## Overview

Offload must retry transient provider failures (sandbox creation, image preparation) automatically with exponential backoff. The mechanism must be centralized — providers themselves are unmodified.

## Error classification

The following `ProviderError` variants are retryable:

- `Timeout`
- `Connection`
- `SandboxExhausted`

All other variants (`CreateFailed`, `ExecFailed`, `DownloadFailed`, `NotFound`, `Io`, `Other`) are non-retryable. This codifies the existing doc comment at `src/provider.rs:44-47`.

This policy is expressed inline at each retry call site via backon's `.when()` combinator — no method or function is added to `ProviderError`. For example:

```rust
{ || inner.create_sandbox(...) }
    .retry(backoff)
    .when(|e| matches!(e, ProviderError::Timeout | ProviderError::Connection | ProviderError::SandboxExhausted))
    .await
```

The same `.when()` pattern is used for `prepare()`. `ProviderError` itself is not modified.

## Dependencies

Add crate `backon` version `1` to `Cargo.toml`. Backon is async-first, supports conditional retries via `.when()`, notification callbacks via `.notify()`, and ownership-passing retries via `RetryableWithContext`. It uses tokio sleep by default.

## Configuration

Retry behavior is controlled by two constants defined in `src/provider/retry.rs` — no user-facing config fields are added:

```rust
const PROVIDER_RETRIES: usize = 2;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
```

## Core type: `RetryProvider<P>`

New file: `src/provider/retry.rs`. Register as `pub mod retry` in `src/provider.rs`. Re-export `RetryProvider` from `src/lib.rs`.

### `RetryProvider<P: SandboxProvider>`

A decorator that implements `SandboxProvider` by delegating to an inner provider `P`. It holds a `backon::ExponentialBuilder` directly — there is no custom `RetryConfig` wrapper type.

The `ExponentialBuilder` is constructed internally using the `PROVIDER_RETRIES` and `RETRY_BASE_DELAY` constants — no external configuration is passed in.

**`prepare(&mut self, ...)`** — Retries on `is_retryable()` errors using backon's `RetryableWithContext` (required because the method takes `&mut self`; the inner provider is passed as owned context through each attempt). Non-retryable errors propagate immediately. Each retry logs via `tracing::warn!`.

**`create_sandbox(&self, ...)`** — Retries on `is_retryable()` errors using backon's `Retryable` trait (closure captures `&self`). Same immediate propagation and logging behavior.

**`base_env(&self)`** — Pure delegation, no retry.

### Retry scope

| `SandboxProvider` method | Retried | Rationale |
|--------------------------|---------|-----------|
| `prepare()` | Yes | Network/API failures during image build |
| `create_sandbox()` | Yes | Network/API failures during sandbox provisioning |

| `Sandbox` trait method | Retried | Rationale |
|------------------------|---------|-----------|
| `exec_stream()` | No | Test execution; handled by test-level retries |
| `download()` | No | Handled at batch level |
| `terminate()` | No | Already fire-and-forget |

## Integration in `main.rs`

In `run_tests()`, construct `RetryProvider` directly — no config fields are read, as retry behavior is governed by constants in `retry.rs`.

- **`ProviderConfig::Default`**: Wrap `DefaultProvider` in `RetryProvider` before calling `prepare()`.
- **`ProviderConfig::Modal`**: Wrap `ModalProvider` in `RetryProvider` before calling `prepare()`.
- **`ProviderConfig::Local`**: No wrapping (no network calls).

## Required tests

Unit tests in `src/provider/retry.rs` using a `MockProvider` with an `AtomicUsize` call counter:

1. Non-retryable error propagates immediately after 1 attempt.
2. Retryable error exhausts all retries, then returns the error.
3. Retryable error on first N attempts, then succeeds — verify success and attempt count.
4. Same three patterns for `prepare()`.
5. `max_retries = 0` disables retries — first error propagates.

Unit tests in `src/provider/retry.rs` covering the `.when()` predicate: one assertion per `ProviderError` variant confirming retryable vs. non-retryable behavior.

## Files touched

| File | Change |
|------|--------|
| `Cargo.toml` | Add `backon = "1"` |
| `src/provider.rs` | Add `pub mod retry;` only — no changes to `ProviderError` |
| `src/config/schema.rs` | No changes |
| `src/provider/retry.rs` | New file: `RetryProvider<P>`, unit tests |
| `src/main.rs` | Wrap Default/Modal providers in `RetryProvider` |
| `src/lib.rs` | Re-export `RetryProvider` |

## Verification

All of the following must pass after implementation:

1. `cargo fmt --check`
2. `cargo clippy` (no warnings)
3. `cargo nextest run`
