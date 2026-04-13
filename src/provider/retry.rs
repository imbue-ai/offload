//! Provider-level retry for transient failures.
//!
//! This module exposes a single public API: the [`with_retry!`] macro.
//! All other items (`backoff_iter`, `is_retryable`) are `#[doc(hidden)]`
//! implementation details required to be `pub` because `#[macro_export]`
//! macros expand at the call site.

use std::time::Duration;

use backon::ExponentialBuilder;

use super::ProviderError;

/// Maximum number of retry attempts (after the initial try).
const PROVIDER_RETRIES: usize = 2;

/// Base delay for exponential backoff.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

fn backoff() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(RETRY_BASE_DELAY)
        .with_max_times(PROVIDER_RETRIES)
}

/// Returns an iterator of backoff durations.
///
/// **Do not call directly** — this is an implementation detail of [`with_retry!`].
#[doc(hidden)]
pub fn backoff_iter() -> impl Iterator<Item = Duration> {
    use backon::BackoffBuilder;
    backoff().build()
}

/// Returns `true` if the error is transient and safe to retry.
///
/// **Do not call directly** — this is an implementation detail of [`with_retry!`].
#[doc(hidden)]
pub fn is_retryable(e: &ProviderError) -> bool {
    matches!(
        e,
        ProviderError::Timeout(_)
            | ProviderError::Connection(_)
            | ProviderError::SandboxExhausted(_)
            | ProviderError::ExecFailed(_)
            | ProviderError::DownloadFailed(_)
            | ProviderError::Io(_)
    )
}

/// Retries an async expression that returns `Result<T, ProviderError>` with
/// exponential backoff.
///
/// Non-retryable errors propagate immediately. Retryable errors are retried
/// up to `PROVIDER_RETRIES` times with exponential backoff.
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retryable_variant_is_retryable() {
        assert!(is_retryable(&ProviderError::Timeout("t".into())));
    }

    #[test]
    fn non_retryable_variant_is_not_retryable() {
        assert!(!is_retryable(&ProviderError::CreateFailed("c".into())));
    }

    // ── with_retry! macro behavior ───────────────────────────────────

    #[tokio::test]
    async fn retryable_error_exhausts_all_retries() {
        let mut attempts = 0u32;
        let result: Result<(), ProviderError> = with_retry!({
            attempts += 1;
            async { Err::<(), _>(ProviderError::Timeout("transient".into())) }
        });
        assert_eq!(attempts, (PROVIDER_RETRIES + 1) as u32);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn retryable_error_then_success() {
        let mut attempts = 0u32;
        let result: Result<&str, ProviderError> = with_retry!({
            attempts += 1;
            async {
                if attempts < 2 {
                    Err(ProviderError::Timeout("transient".into()))
                } else {
                    Ok("done")
                }
            }
        });
        assert_eq!(attempts, 2);
        assert_eq!(result.unwrap(), "done");
    }
}
