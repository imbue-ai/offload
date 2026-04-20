//! Flexible parallel test runner with pluggable execution providers.

pub mod bundled;
pub mod config;
pub mod connector;
pub mod framework;
pub mod history;
pub mod orchestrator;
pub mod provider;
pub mod report;
pub mod trace;

// Re-export commonly used types for convenience.
// These are the types most users will need when setting up offload.

pub use config::{Config, load_config};
pub use framework::{TestFramework, TestInstance, TestOutcome, TestRecord, TestResult};
pub use orchestrator::{Orchestrator, RunResult, SandboxPool};
pub use provider::{Sandbox, SandboxProvider};
pub use report::print_summary;
pub use trace::Tracer;

/// Base62 alphabet for run ID generation.
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Generates a 4-character base62 run ID for correlating test results.
///
/// Run IDs are used to group test attempts from the same `offload run` invocation.
/// With 4 characters from a 62-character alphabet, there are ~14 million unique IDs.
pub fn generate_run_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..4)
        .map(|_| BASE62[rng.random_range(0..62)] as char)
        .collect()
}
