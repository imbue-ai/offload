//! Flexible parallel test runner with pluggable execution providers.

pub mod bundled;
pub mod cache;
pub mod config;
pub mod connector;
pub mod framework;
pub mod orchestrator;
pub mod provider;
pub mod report;

// Re-export commonly used types for convenience.
// These are the types most users will need when setting up offload.

pub use config::{Config, load_config};
pub use framework::{TestFramework, TestInstance, TestOutcome, TestRecord, TestResult};
pub use orchestrator::{Orchestrator, RunResult, SandboxPool};
pub use provider::{Sandbox, SandboxProvider};
pub use report::print_summary;
