# Spec: Provider-level retry for transient failures

## Overview

Offload shall retry transient provider failures (sandbox creation, image preparation, result download) automatically with exponential backoff. The mechanism will use a `with_retry!` macro defined in `src/provider/retry.rs`, exported as `pub` via `#[macro_export]` so that both the library crate and the binary crate (`src/main.rs`) can use it uniformly. All retry internals (backoff timing, retryability classification, backon dependency) shall be encapsulated inside the retry module. The macro's helper functions (`backoff_iter()`, `is_retryable()`) are `pub` only because `#[macro_export]` macros expand at the call site and require visible paths; they are marked `#[doc(hidden)]` and are not part of the intended API.

## Error classification

The following `ProviderError` variants are retryable:

- `Timeout` -- transient by nature.
- `Connection` -- transient by nature.
- `SandboxExhausted` -- transient (capacity).
- `ExecFailed` -- wraps shell commands that make network calls to cloud APIs (e.g. `modal_sandbox.py create`). Transient network errors (DNS timeout, connection reset, HTTP 503) produce non-zero exit codes mapped to this variant.
- `DownloadFailed` -- wraps file-transfer shell commands. Connection resets and timeouts during download surface here. The download is idempotent and safe to retry.
- `Io` -- has `#[from] std::io::Error`. Many `io::Error` kinds are transient (`EMFILE`, `EAGAIN`, network filesystem timeouts).

The following `ProviderError` variants are non-retryable:

- `CreateFailed` -- semantically permanent (image build or config error).
- `NotFound` -- semantically permanent (sandbox does not exist).
- `Other` -- has `#[from] anyhow::Error`, acts as a catch-all. Non-retryable because the variant is too broad to classify safely. Future work should consider removing the `#[from]` attribute or splitting into more specific error types.

### Predicate

The retryability policy shall be expressed via an `is_retryable()` function in `src/provider/retry.rs`. It is `pub` + `#[doc(hidden)]` (required for macro expansion at external call sites) but is not part of the intended API — call sites shall not use it directly.

```rust
fn is_retryable(e: &ProviderError) -> bool {
    matches!(e,
        ProviderError::Timeout(_)
        | ProviderError::Connection(_)
        | ProviderError::SandboxExhausted(_)
        | ProviderError::ExecFailed(_)
        | ProviderError::DownloadFailed(_)
        | ProviderError::Io(_)
    )
}
```

Do not modify `ProviderError` itself.

## Dependencies

Add crate `backon` version `1` to `Cargo.toml`. Only `src/provider/retry.rs` shall import backon — no other file in the codebase should reference it. Backon will be used solely for its `ExponentialBuilder` to generate backoff durations.

> **Warning:** Do not use backon's `Retryable` or `RetryableWithContext` traits at call sites. Do not import backon outside of `retry.rs`. All backon usage shall be internal to the retry module.

## Configuration

Retry behavior shall be controlled by two constants defined in `src/provider/retry.rs` -- no user-facing config fields will be added:

```rust
const PROVIDER_RETRIES: usize = 2;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
```

## Design: `with_retry!` macro

Create a new file `src/provider/retry.rs`. Register it as `pub mod retry` in `src/provider.rs`. Do not re-export from `src/lib.rs`.

The module's sole API shall be a `with_retry!` macro, exported as `pub` via `#[macro_export]`. This is required so that the binary crate (`src/main.rs`) can use the same macro as library-crate call sites, avoiding a separate wrapper function or trait method. `#[macro_export]` places the macro at the crate root (`offload::with_retry`); the `pub use with_retry;` inside the module creates a secondary path (`offload::provider::retry::with_retry`) for ergonomic imports.

The helper functions `backoff_iter()` and `is_retryable()` must be `pub` (not `pub(crate)`) because `#[macro_export]` macros expand in the caller's crate, which requires the referenced symbols to be externally visible. They are annotated `#[doc(hidden)]` to signal they are not intended for direct use.

> **Warning:** Do not write a `prepare_with_retry()` trait method or wrapper function. The macro is the single, uniform retry mechanism for all call sites — library and binary crate alike.

### Why a macro, not a function

> **Warning:** Do not implement this as a generic async function. `async fn with_retry<F, Fut>(f: F)` where `F: FnMut() -> Fut` works for `&self` methods but **not** for `&mut self` methods. The returned future would borrow from the closure's captured `&mut self`, but `FnMut`'s associated output type cannot express a lifetime tied to `&mut self` — this is the lending-closure gap in stable Rust. This will cause compilation errors at the `prepare()` call sites.

> **Warning:** Do not use backon's `RetryableWithContext` to work around this. It requires passing ownership of the provider through each retry attempt, which unnecessarily complicates call sites and forces restructuring of the `tokio::try_join!` blocks in `main.rs`. A plain retry loop reborrows `&mut self` each iteration, which is simpler and correct.

A macro sidesteps both problems: it expands a retry loop in the caller's scope, where `&mut self` is reborrowed naturally each iteration. One mechanism covers both `&self` and `&mut self` methods uniformly.

### Module contents

```rust
use std::time::Duration;
use backon::ExponentialBuilder;
use super::ProviderError;

const PROVIDER_RETRIES: usize = 2;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

fn backoff() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(RETRY_BASE_DELAY)
        .with_max_times(PROVIDER_RETRIES)
}

#[doc(hidden)]
pub fn is_retryable(e: &ProviderError) -> bool {
    matches!(e,
        ProviderError::Timeout(_)
        | ProviderError::Connection(_)
        | ProviderError::SandboxExhausted(_)
        | ProviderError::ExecFailed(_)
        | ProviderError::DownloadFailed(_)
        | ProviderError::Io(_)
    )
}

#[doc(hidden)]
pub fn backoff_iter() -> impl Iterator<Item = Duration> {
    use backon::BackoffBuilder;
    backoff().build()
}

#[macro_export]
macro_rules! with_retry {
    ($expr:expr) => {{
        let mut backoff = $crate::provider::retry::backoff_iter();
        loop {
            match $expr.await {
                Ok(v) => break Ok(v),
                Err(e) if !$crate::provider::retry::is_retryable(&e) => break Err(e),
                Err(e) => match backoff.next() {
                    Some(dur) => {
                        tracing::warn!("retrying after {:?}: {}", dur, e);
                        tokio::time::sleep(dur).await;
                    }
                    None => break Err(e),
                },
            }
        }
    }};
}
pub use with_retry;
```

Note: `backoff_iter()` and `is_retryable()` must be `pub` so the macro can reference them when expanded in external crates. They are annotated `#[doc(hidden)]` to signal they are internal — the macro is the only entry point callers should use. `#[macro_export]` places the macro at the crate root; the `pub use with_retry;` creates a re-export path at `offload::provider::retry::with_retry` for convenience.

### Call site pattern

All call sites — library crate and binary crate alike — use the macro uniformly:

```rust
// Library crate (pool.rs, runner.rs)
use crate::provider::retry::with_retry;

with_retry!(provider.create_sandbox(&cfg))?;
with_retry!(sandbox.download(&file_pairs))?;

// Binary crate (main.rs)
use offload::with_retry;

with_retry!(provider.prepare(&dirs, no_cache, init, done))
    .context("Failed to prepare")?;
```

No backon imports, no backoff configuration, no retryability checks at any call site.

> **Warning:** Do not write manual retry loops at call sites. Do not import `backoff_iter()` or `is_retryable()` directly. Do not write wrapper functions or trait methods for retry — use the `with_retry!` macro everywhere.

## Retry scope

| `SandboxProvider` method | Retried | Rationale |
|--------------------------|---------|-----------|
| `prepare()` | Yes | Network/API failures during image build |
| `create_sandbox()` | Yes | Network/API failures during sandbox provisioning |

| `Sandbox` trait method | Retried | Rationale |
|------------------------|---------|-----------|
| `download()` | Yes | Network/API failures during result retrieval; no batch-level retry exists |
| `exec_stream()` | No | Test execution; handled by test-level retries |
| `terminate()` | No | Already fire-and-forget |

Note on `download()`: there is no batch-level retry for downloads. `try_download_results()` in `runner.rs` returns `None` on failure, silently losing results. Test-level retry re-executes the test, not the download. Per-call retry is the only protection.

## Integration

Retry shall be applied at three call sites. The local provider needs no retry (no network calls).

### `src/main.rs` -- `prepare()` calls

`main.rs` is a binary crate. It imports the macro via `use offload::with_retry;` (the `#[macro_export]` crate-root path) and wraps `prepare()` calls directly.

For both `ProviderConfig::Default` and `ProviderConfig::Modal`:

```rust
use offload::with_retry;

let image_id = with_retry!(provider.prepare(
    &copy_dir_tuples,
    no_cache,
    config.offload.sandbox_init_cmd.as_deref(),
    Some(&discovery_done),
))
.context("Failed to prepare Default provider")?;
Ok(image_id)
```

Note: inside `tokio::try_join!` blocks, the macro's `loop` expansion can prevent the compiler from inferring the error type. Binding the result to a local variable with `?` and returning `Ok(val)` resolves this.

`ProviderConfig::Local` shall remain unchanged (no network calls to retry).

### `src/orchestrator/pool.rs` -- `create_sandbox()` call

```rust
use crate::provider::retry::with_retry;

with_retry!(provider.create_sandbox(&cfg))?;
```

### `src/orchestrator/runner.rs` -- `download()` call

```rust
use crate::provider::retry::with_retry;

with_retry!(self.sandbox.download(&path_pairs))?;
```

## Required tests

All tests shall go in `src/provider/retry.rs`.

### `is_retryable()` tests

One assertion per `ProviderError` variant confirming retryable vs. non-retryable classification:

- `Timeout` -- retryable
- `Connection` -- retryable
- `SandboxExhausted` -- retryable
- `ExecFailed` -- retryable
- `DownloadFailed` -- retryable
- `Io` -- retryable
- `CreateFailed` -- non-retryable
- `NotFound` -- non-retryable
- `Other` -- non-retryable

### `with_retry!` macro behavior tests

Using a counter to track attempts:

1. **Non-retryable error propagates immediately.** Expression returns `CreateFailed` on every call. Verify exactly 1 attempt and the error is returned.
2. **Retryable error exhausts all retries.** Expression returns `Timeout` on every call. Verify exactly `PROVIDER_RETRIES + 1` attempts (1 initial + 2 retries) and the error is returned.
3. **Retryable error then success.** Expression returns `Timeout` for the first N calls, then returns `Ok(value)`. Verify the final result is `Ok(value)` and the attempt count matches N + 1.

## Files touched

| File | Change |
|------|--------|
| `Cargo.toml` | Add `backon = "1"` |
| `src/provider.rs` | Add `pub mod retry;` |
| `src/provider/retry.rs` | New file: `with_retry!` macro (`#[macro_export]`, `pub use`), `pub` + `#[doc(hidden)]` helpers, unit tests |
| `src/main.rs` | Import `offload::with_retry`, wrap both `prepare()` calls with `with_retry!` |
| `src/orchestrator/pool.rs` | Wrap `create_sandbox()` call with `with_retry!` |
| `src/orchestrator/runner.rs` | Wrap `download()` call with `with_retry!` |

Note: `src/lib.rs` shall not be touched (no re-export needed — `#[macro_export]` places the macro at the crate root automatically). `src/config/schema.rs` shall not be touched. No file outside `src/provider/retry.rs` shall import backon. The `SandboxProvider` trait shall not have a `prepare_with_retry()` method.

## Verification

All of the following must pass after implementation:

1. `cargo fmt --check`
2. `cargo clippy` (no warnings)
3. `cargo nextest run`
