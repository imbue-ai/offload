//! shotgun-rs: A flexible parallel test runner.
//!
//! This crate provides a framework for running tests in parallel across
//! multiple sandboxes (containers, VMs, or local processes).
//!
//! # Architecture
//!
//! The main components are:
//!
//! - **Providers**: Create and manage sandboxes (Docker, SSH, Process)
//! - **Discovery**: Find tests to run (pytest, cargo test, generic)
//! - **Executor**: Coordinate test distribution and execution
//! - **Report**: Generate test reports (JUnit XML, console)
//!
//! # Example
//!
//! ```no_run
//! use shotgun::config::load_config;
//! use shotgun::executor::Orchestrator;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = load_config(std::path::Path::new("shotgun.toml"))?;
//!     // ... set up provider, discoverer, reporter ...
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod connector;
pub mod discovery;
pub mod executor;
pub mod provider;
pub mod report;

// Re-export commonly used types
pub use config::{load_config, Config};
pub use discovery::{TestCase, TestDiscoverer, TestOutcome, TestResult};
pub use executor::{Orchestrator, RunResult};
pub use provider::{Sandbox, SandboxProvider};
pub use report::Reporter;
