//! # offload-rs
//!
//! A flexible, high-performance parallel test runner that executes tests across
//! multiple isolated sandboxes with pluggable execution providers.
//!
//! ## Overview
//!
//! Offload enables distributed test execution across local processes, or custom
//! cloud providers (like Modal). It provides:
//!
//! - **Parallel execution** across multiple isolated sandbox environments
//! - **Automatic test discovery** for pytest, cargo test, and custom frameworks
//! - **Flaky test detection** with configurable retry logic
//! - **JUnit XML reporting** for CI/CD integration
//! - **Streaming output** for real-time test progress
//!
//! ## Architecture
//!
//! The crate is organized into four main subsystems:
//!
//! ### Providers ([`provider`])
//!
//! Providers create and manage sandbox execution environments. Each provider
//! implements the [`SandboxProvider`] trait:
//!
//! - [`provider::local::LocalProvider`] - Run tests as local processes
//! - [`provider::default::DefaultProvider`] - Run tests using custom shell commands
//!
//! ### Framework ([`framework`])
//!
//! Frameworks find tests and generate commands to run them. Each framework
//! implements the [`TestFramework`] trait:
//!
//! - [`framework::pytest::PytestFramework`] - Discover and run pytest tests
//! - [`framework::cargo::CargoFramework`] - Discover and run Rust tests
//! - [`framework::default::DefaultFramework`] - Custom framework via shell commands
//!
//! ### Orchestrator ([`orchestrator`])
//!
//! The orchestrator module coordinates test distribution and execution:
//!
//! - [`Orchestrator`] - Main entry point that coordinates the entire test run
//! - [`orchestrator::Scheduler`] - Distributes tests across available sandboxes
//! - [`orchestrator::TestRunner`] - Executes tests within a single sandbox
//!
//! ### Reporting ([`report`])
//!
//! Utilities for test result reporting:
//!
//! - [`report::print_summary`] - Print test results to console
//! - [`report::MasterJunitReport`] - JUnit XML report generation
//!
//! ## Quick Start
//!
//! ```no_run
//! use offload::config::{load_config, SandboxConfig};
//! use offload::orchestrator::{Orchestrator, SandboxPool};
//! use offload::provider::local::LocalProvider;
//! use offload::framework::{TestFramework, pytest::PytestFramework};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Load configuration from TOML file
//!     let config = load_config(std::path::Path::new("offload.toml"))?;
//!
//!     // Create provider (runs tests as local processes)
//!     let provider = LocalProvider::new(Default::default());
//!
//!     // Create framework (finds pytest tests)
//!     let framework = PytestFramework::new(Default::default());
//!
//!     // Discover tests using the framework
//!     let tests = framework.discover(&[]).await?;
//!
//!     // Pre-populate sandbox pool
//!     let sandbox_config = SandboxConfig {
//!         id: "sandbox".to_string(),
//!         working_dir: None,
//!         env: vec![],
//!         copy_dirs: vec![],
//!     };
//!     let mut sandbox_pool = SandboxPool::new();
//!     sandbox_pool.populate(config.offload.max_parallel, &provider, &sandbox_config).await?;
//!
//!     // Run tests using the orchestrator
//!     let orchestrator = Orchestrator::new(config, framework, false);
//!     let result = orchestrator.run_with_tests(&tests, sandbox_pool).await?;
//!
//!     std::process::exit(result.exit_code());
//! }
//! ```
//!
//! ## Configuration
//!
//! Offload is configured via TOML files. See [`config`] module for schema details.
//!
//! ## Custom Providers
//!
//! You can implement custom providers for cloud platforms like Modal, AWS Lambda,
//! or Kubernetes by implementing the [`SandboxProvider`] and [`Sandbox`] traits,
//! or by using the [`provider::default::DefaultProvider`] with custom shell commands.
//!
//! [`SandboxProvider`]: provider::SandboxProvider
//! [`Sandbox`]: provider::Sandbox
//! [`TestFramework`]: framework::TestFramework
//! [`Orchestrator`]: orchestrator::Orchestrator

pub mod bundled;
pub mod cache;
pub mod config;
pub mod connector;
pub mod framework;
pub mod orchestrator;
pub mod profiling;
pub mod provider;
pub mod report;

// Re-export commonly used types for convenience.
// These are the types most users will need when setting up offload.

pub use config::{Config, load_config};
pub use framework::{TestFramework, TestInstance, TestOutcome, TestRecord, TestResult};
pub use orchestrator::{Orchestrator, RunResult, SandboxPool};
pub use provider::{Sandbox, SandboxProvider};
pub use report::print_summary;
