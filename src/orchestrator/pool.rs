//! Sandbox pool for reusing sandboxes across test runs.
//!
//! The [`SandboxPool`] holds sandboxes that can be reused between the initial
//! test run and retry attempts, avoiding the overhead of creating new sandboxes.

use crate::config::SandboxConfig;
use crate::provider::retry::with_retry;
use crate::provider::{ProviderError, Sandbox, SandboxProvider};
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
    ) -> Result<(), ProviderError>
    where
        P: SandboxProvider<Sandbox = S>,
    {
        let progress = indicatif::ProgressBar::new(count as u64);
        if let Ok(style) = indicatif::ProgressStyle::default_bar().template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} Creating sandboxes...",
        ) {
            progress.set_style(style.progress_chars("#>-"));
        }
        progress.enable_steady_tick(std::time::Duration::from_millis(100));

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
        Ok(())
    }

    /// Takes all sandboxes out of the pool for parallel execution.
    ///
    /// The pool will be empty after this call. Use [`return_all`](Self::return_all)
    /// to return sandboxes after use.
    pub fn take_all(&mut self) -> Vec<S> {
        std::mem::take(&mut self.sandboxes)
    }
}

impl<S: Sandbox> Default for SandboxPool<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CostEstimate, OutputStream, ProviderResult};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    struct FakeSandbox {
        id: String,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        async fn exec_stream(
            &self,
            _cmd: &crate::provider::Command,
        ) -> ProviderResult<OutputStream> {
            unimplemented!()
        }
        async fn download(&self, _paths: &[(&Path, &Path)]) -> ProviderResult<()> {
            unimplemented!()
        }
        async fn terminate(&self) -> ProviderResult<()> {
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

        async fn prepare(
            &mut self,
            _copy_dirs: &[(PathBuf, PathBuf)],
            _no_cache: bool,
            _sandbox_init_cmd: Option<&str>,
            _discovery_done: Option<&AtomicBool>,
        ) -> ProviderResult<Option<String>> {
            Ok(None)
        }

        async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<FakeSandbox> {
            Ok(FakeSandbox {
                id: config.id.clone(),
            })
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
        pool.populate(4, &FakeProvider, &config).await?;

        let sandboxes = pool.take_all();
        assert_eq!(sandboxes.len(), 4);

        // All sandbox IDs must be unique
        let ids: std::collections::HashSet<_> =
            sandboxes.iter().map(|s| s.id().to_string()).collect();
        assert_eq!(ids.len(), 4, "expected 4 unique sandbox IDs, got {:?}", ids);
        Ok(())
    }
}
