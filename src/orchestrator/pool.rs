//! Sandbox pool for reusing sandboxes across test runs.
//!
//! The [`SandboxPool`] holds sandboxes that can be reused between the initial
//! test run and retry attempts, avoiding the overhead of creating new sandboxes.

use crate::provider::Sandbox;

/// A pool of reusable sandboxes.
///
/// Sandboxes are added to the pool after initial test execution and can be
/// reused for retry attempts. The pool manages sandbox lifecycle and provides
/// methods to take and return sandboxes.
///
/// # Example
///
/// ```ignore
/// let mut pool = SandboxPool::new();
///
/// // After initial batch execution, add sandboxes to pool
/// pool.add(sandbox);
///
/// // For retries, take all sandboxes
/// let sandboxes = pool.take_all();
/// // ... run retries in parallel ...
/// pool.return_all(sandboxes);
/// ```
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

    /// Creates a new pool with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            sandboxes: Vec::with_capacity(capacity),
        }
    }

    /// Adds a sandbox to the pool.
    pub fn add(&mut self, sandbox: S) {
        self.sandboxes.push(sandbox);
    }

    /// Takes one sandbox from the pool, if available.
    pub fn take_one(&mut self) -> Option<S> {
        self.sandboxes.pop()
    }

    /// Takes all sandboxes out of the pool for parallel execution.
    ///
    /// The pool will be empty after this call. Use [`return_all`](Self::return_all)
    /// to return sandboxes after use.
    pub fn take_all(&mut self) -> Vec<S> {
        std::mem::take(&mut self.sandboxes)
    }

    /// Returns sandboxes to the pool after use.
    pub fn return_all(&mut self, sandboxes: Vec<S>) {
        self.sandboxes.extend(sandboxes);
    }

    /// Returns the number of available sandboxes in the pool.
    pub fn len(&self) -> usize {
        self.sandboxes.len()
    }

    /// Returns true if the pool has no sandboxes.
    pub fn is_empty(&self) -> bool {
        self.sandboxes.is_empty()
    }

    /// Terminates all sandboxes in the pool.
    ///
    /// This should be called when the pool is no longer needed to clean up
    /// resources. Errors during termination are logged but don't prevent
    /// other sandboxes from being terminated.
    pub async fn terminate_all(&mut self) {
        for sandbox in self.sandboxes.drain(..) {
            if let Err(e) = sandbox.terminate().await {
                tracing::warn!("Failed to terminate sandbox {}: {}", sandbox.id(), e);
            }
        }
    }
}

impl<S: Sandbox> Default for SandboxPool<S> {
    fn default() -> Self {
        Self::new()
    }
}
