//! Sandbox pool for reusing sandboxes across test runs.
//!
//! The [`SandboxPool`] holds sandboxes that can be reused between the initial
//! test run and retry attempts, avoiding the overhead of creating new sandboxes.

use crate::config::SandboxConfig;
use crate::provider::retry::with_retry;
use crate::provider::{ProviderError, ProviderResult, Sandbox, SandboxProvider};
use futures::StreamExt;
use futures::stream::FuturesUnordered;

/// A pool of reusable sandboxes.
///
/// Sandboxes are added to the pool after initial test execution and can be
/// reused for retry attempts. The pool manages sandbox lifecycle and provides
/// methods to take and return sandboxes.
pub struct SandboxPool<S: Sandbox> {
    sandboxes: Vec<S>,
}

impl<S: Sandbox> SandboxPool<S> {
    /// Creates a new empty sandbox pool.
    pub fn new() -> Self {
        Self {
            sandboxes: Vec::new(),
        }
    }

    /// Populates the pool by creating sandboxes concurrently using the given provider.
    ///
    /// Creates `count` sandboxes in parallel, failing fast if any creation fails.
    ///
    /// # Errors
    ///
    /// Returns the first error encountered during sandbox creation.
    pub async fn populate<P>(
        &mut self,
        count: usize,
        provider: &P,
        config: &SandboxConfig,
        ci: bool,
    ) -> Result<(), ProviderError>
    where
        P: SandboxProvider<Sandbox = S>,
    {
        let progress = if ci {
            eprintln!("Creating {count} sandboxes...");
            indicatif::ProgressBar::hidden()
        } else {
            let pb = indicatif::ProgressBar::new(count as u64);
            if let Ok(style) = indicatif::ProgressStyle::default_bar().template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} Creating sandboxes...",
            ) {
                pb.set_style(style.progress_chars("#>-"));
            }
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        };

        let futs: FuturesUnordered<_> = (0..count)
            .map(|i| {
                let mut cfg = config.clone();
                cfg.id = format!("{}-{}", config.id, i);
                async move { with_retry!(provider.create_sandbox(&cfg)) }
            })
            .collect();

        futures::pin_mut!(futs);
        while let Some(result) = futs.next().await {
            match result {
                Ok(sandbox) => {
                    self.sandboxes.push(sandbox);
                    progress.inc(1);
                }
                Err(e) => {
                    progress.finish_and_clear();
                    return Err(e);
                }
            }
        }
        progress.finish_and_clear();
        if ci {
            eprintln!("Sandboxes created.");
        }
        Ok(())
    }

    /// Takes all sandboxes out of the pool for parallel execution.
    ///
    /// The pool will be empty after this call. Use [`return_all`](Self::return_all)
    /// to return sandboxes after use.
    pub fn take_all(&mut self) -> Vec<S> {
        std::mem::take(&mut self.sandboxes)
    }

    /// Terminates every sandbox in the pool, consuming it.
    ///
    /// Used to tear down a pre-warmed pool that will never run tests (discovery
    /// failed or produced no tests) so the sandboxes do not leak. Returns one
    /// result per sandbox; termination is best-effort.
    pub async fn terminate_all(self) -> Vec<ProviderResult<()>> {
        S::terminate_many(self.sandboxes).await
    }
}

impl<S: Sandbox> Default for SandboxPool<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// Reconcile concurrent test discovery with concurrent pool pre-warming.
///
/// When the pool is pre-warmed concurrently with discovery (via `tokio::join!`),
/// either side may complete or fail independently. This resolves the four
/// outcomes so no pre-warmed sandbox leaks:
///
/// - discovery `Err`: terminate the pool if it was created, then propagate the
///   discovery error (also propagated deterministically when both sides fail);
/// - discovery `Ok` but pool `Err`: propagate the pool error (no pool to clean);
/// - discovery `Ok` with no tests: terminate the pool and return `Ok(None)`;
/// - discovery `Ok` with tests and pool `Ok`: return `Ok(Some((tests, pool)))`.
pub async fn resolve_prewarm<S: Sandbox, T>(
    tests: anyhow::Result<Vec<T>>,
    pool: anyhow::Result<SandboxPool<S>>,
) -> anyhow::Result<Option<(Vec<T>, SandboxPool<S>)>> {
    match (tests, pool) {
        (Err(discovery_err), Ok(pool)) => {
            pool.terminate_all().await;
            Err(discovery_err)
        }
        (Err(discovery_err), Err(_)) => Err(discovery_err),
        (Ok(_), Err(pool_err)) => Err(pool_err),
        (Ok(tests), Ok(pool)) => {
            if tests.is_empty() {
                pool.terminate_all().await;
                Ok(None)
            } else {
                Ok(Some((tests, pool)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::TestRecord;
    use crate::provider::{CostEstimate, OutputStream, PrepareContext};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fake sandbox that records terminations into a shared counter.
    ///
    /// The counter is an `Arc<AtomicUsize>` because [`Sandbox`] requires `Send`
    /// and the counter is shared across every sandbox in a pool plus the test.
    struct FakeSandbox {
        id: String,
        terminate_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        async fn exec_stream(
            &mut self,
            _cmd: &crate::provider::Command,
        ) -> ProviderResult<(OutputStream, tokio::process::Child)> {
            unimplemented!()
        }
        async fn download(&mut self, _paths: &[(&Path, &Path)]) -> ProviderResult<()> {
            unimplemented!()
        }
        async fn terminate(self) -> ProviderResult<()> {
            self.terminate_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        fn cost_estimate(&self) -> CostEstimate {
            CostEstimate::default()
        }
    }

    struct FakeProvider;

    #[async_trait]
    impl SandboxProvider for FakeProvider {
        type Sandbox = FakeSandbox;

        async fn prepare(&mut self, _ctx: &PrepareContext<'_>) -> ProviderResult<Option<String>> {
            Ok(None)
        }

        async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<FakeSandbox> {
            Ok(FakeSandbox {
                id: config.id.clone(),
                terminate_count: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    /// Builds a pool of `size` sandboxes that share `counter` for termination
    /// accounting, bypassing the provider so tests are deterministic.
    fn populated_pool(size: usize, counter: &Arc<AtomicUsize>) -> SandboxPool<FakeSandbox> {
        SandboxPool {
            sandboxes: (0..size)
                .map(|i| FakeSandbox {
                    id: format!("sb-{i}"),
                    terminate_count: Arc::clone(counter),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn test_populate_creates_unique_sandbox_ids() -> anyhow::Result<()> {
        let mut pool = SandboxPool::new();
        let config = SandboxConfig {
            id: "offload-test".to_string(),
            working_dir: None,
            env: vec![],
            copy_dirs: vec![],
        };
        pool.populate(4, &FakeProvider, &config, false).await?;

        let sandboxes = pool.take_all();
        assert_eq!(sandboxes.len(), 4);

        // All sandbox IDs must be unique
        let ids: std::collections::HashSet<_> =
            sandboxes.iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids.len(), 4, "expected 4 unique sandbox IDs, got {:?}", ids);
        Ok(())
    }

    #[tokio::test]
    async fn test_terminate_all_terminates_every_sandbox() -> anyhow::Result<()> {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = populated_pool(4, &counter);

        let results = pool.terminate_all().await;

        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|r| r.is_ok()));
        assert_eq!(counter.load(Ordering::Acquire), 4);
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_prewarm_discovery_error_terminates_pool() -> anyhow::Result<()> {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = populated_pool(3, &counter);
        let tests: anyhow::Result<Vec<TestRecord>> = Err(anyhow::anyhow!("discovery failed"));

        let resolved = resolve_prewarm(tests, Ok(pool)).await;

        assert!(resolved.is_err());
        assert_eq!(counter.load(Ordering::Acquire), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_prewarm_empty_tests_terminates_pool() -> anyhow::Result<()> {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = populated_pool(3, &counter);

        let resolved = resolve_prewarm(Ok(Vec::<TestRecord>::new()), Ok(pool)).await?;

        assert!(resolved.is_none());
        assert_eq!(counter.load(Ordering::Acquire), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_resolve_prewarm_success_keeps_pool() -> anyhow::Result<()> {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = populated_pool(2, &counter);
        let tests = vec![TestRecord::new("test-1", "group-1")];

        let resolved = resolve_prewarm(Ok(tests), Ok(pool)).await?;

        let (returned_tests, returned_pool) =
            resolved.ok_or_else(|| anyhow::anyhow!("expected Some((tests, pool))"))?;
        assert_eq!(returned_tests.len(), 1);
        assert_eq!(returned_pool.sandboxes.len(), 2);
        assert_eq!(counter.load(Ordering::Acquire), 0);
        Ok(())
    }
}
