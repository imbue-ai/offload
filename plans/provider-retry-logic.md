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

A generic `async fn with_retry<F, Fut>(f: F)` where `F: FnMut() -> Fut` works for `&self` methods but not for `&mut self` methods (`prepare()`). The returned future borrows from the closure's captured `&mut self`, but `FnMut::Output` cannot express a lifetime tied to `&mut self` — this is the lending-closure gap in stable Rust. A macro sidesteps this because it expands a retry loop in the caller's scope, where `&mut self` is reborrowed naturally each iteration.

### Macro implementation: two options

The implementer should present both options to the human and let them choose.

#### Option A: Self-contained loop (uniform, minimal API surface)

A single-arm macro with a manual retry loop. Uses backon only for its `ExponentialBuilder` to generate backoff durations. All retry logic is visible in the loop body. The same syntax works for both `&self` and `&mut self` methods.

**Tradeoff:** Does not use backon's `Retryable` / `RetryableWithContext` traits. The loop duplicates what those traits do, but is self-contained — only `backoff_iter()` and `is_retryable()` need to be `#[doc(hidden)] pub`. No backon types appear in the macro expansion.

```rust
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

Call sites (all identical syntax):

```rust
with_retry!(provider.create_sandbox(&cfg))?;
with_retry!(self.sandbox.download(&path_pairs))?;
with_retry!(provider.prepare(&dirs, no_cache, init, done))
    .context("Failed to prepare")?;
```

#### Option B: Two-arm macro using backon traits (idiomatic, larger API surface)

Two macro arms: the default arm uses backon's `Retryable` trait for `&self` methods; a `mut ctx =>` arm uses `RetryableWithContext` for `&mut self` methods, passing ownership through each retry and reassigning the binding afterward.

**Tradeoff:** Uses backon idiomatically, but leaks `backon::Retryable` and `backon::RetryableWithContext` into the macro expansion. These must be re-exported as `#[doc(hidden)] pub` from the retry module so the macro compiles at external call sites. The `&mut self` call sites have a different invocation syntax (`mut provider =>`).

```rust
// Re-exports required for macro expansion at external call sites
#[doc(hidden)]
pub use backon::Retryable as __Retryable;
#[doc(hidden)]
pub use backon::RetryableWithContext as __RetryableWithContext;

#[macro_export]
macro_rules! with_retry {
    // &self methods — Retryable
    ($expr:expr) => {{
        use $crate::provider::retry::{__Retryable, __backoff, is_retryable};
        (|| $expr)
            .retry(__backoff())
            .when(|e| is_retryable(e))
            .await
    }};

    // &mut self methods — RetryableWithContext (pass ownership through)
    (mut $ctx:ident => $expr:expr) => {{
        use $crate::provider::retry::{__RetryableWithContext, __backoff, is_retryable};
        let (ctx, result) = (|mut $ctx| async move {
            let r = $expr.await;
            ($ctx, r)
        })
        .retry(__backoff())
        .when(|e| is_retryable(e))
        .context($ctx)
        .await;
        $ctx = ctx;
        result
    }};
}
pub use with_retry;
```

Call sites:

```rust
// &self — same as Option A
with_retry!(provider.create_sandbox(&cfg))?;
with_retry!(self.sandbox.download(&path_pairs))?;

// &mut self — caller names the context binding
with_retry!(mut provider => provider.prepare(&dirs, no_cache, init, done))
    .context("Failed to prepare")?;
```

### Shared module contents (both options)

Regardless of which macro option is chosen, the module contains these common definitions:

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
```

`#[macro_export]` places the macro at the crate root (`offload::with_retry`); `pub use with_retry;` creates a secondary path at `offload::provider::retry::with_retry`. All `#[doc(hidden)] pub` items are required for macro expansion at external call sites and are not part of the intended API.

> **Warning:** Do not write manual retry loops at call sites. Do not import helpers directly. Do not write wrapper functions or trait methods for retry — use the `with_retry!` macro everywhere.

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
