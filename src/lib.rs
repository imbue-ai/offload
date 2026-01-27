//! # shotgun-rs
//!
//! A flexible, high-performance parallel test runner that executes tests across
//! multiple isolated sandboxes with pluggable execution providers.
//!
//! ## Overview
//!
//! Shotgun enables distributed test execution across Docker containers, SSH-connected
//! remote machines, local processes, or custom cloud providers (like Modal). It provides:
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
//! - [`provider::process::ProcessProvider`] - Run tests as local processes
//! - [`provider::docker::DockerProvider`] - Run tests in Docker containers
//! - [`provider::ssh::SshProvider`] - Run tests on remote machines via SSH
//! - [`provider::default::DefaultProvider`] - Run tests using custom shell commands
//!
//! ### Discovery ([`discovery`])
//!
//! Discoverers find tests and generate commands to run them. Each discoverer
//! implements the [`TestDiscoverer`] trait:
//!
//! - [`discovery::pytest::PytestDiscoverer`] - Discover and run pytest tests
//! - [`discovery::cargo::CargoDiscoverer`] - Discover and run Rust tests
//! - [`discovery::default::DefaultDiscoverer`] - Custom discovery via shell commands
//!
//! ### Executor ([`executor`])
//!
//! The executor module coordinates test distribution and execution:
//!
//! - [`Orchestrator`] - Main entry point that coordinates the entire test run
//! - [`executor::Scheduler`] - Distributes tests across available sandboxes
//! - [`executor::TestRunner`] - Executes tests within a single sandbox
//! - [`executor::RetryManager`] - Handles retry logic and flaky test detection
//!
//! ### Reporting ([`report`])
//!
//! Reporters receive events during test execution:
//!
//! - [`report::ConsoleReporter`] - Terminal output with progress bar
//! - [`report::JUnitReporter`] - Generate JUnit XML for CI systems
//! - [`report::MultiReporter`] - Combine multiple reporters
//!
//! ## Quick Start
//!
//! ```no_run
//! use shotgun::config::load_config;
//! use shotgun::executor::Orchestrator;
//! use shotgun::provider::process::ProcessProvider;
//! use shotgun::discovery::pytest::PytestDiscoverer;
//! use shotgun::report::{ConsoleReporter, MultiReporter, JUnitReporter};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Load configuration from TOML file
//!     let config = load_config(std::path::Path::new("shotgun.toml"))?;
//!
//!     // Create provider (runs tests as local processes)
//!     let provider = ProcessProvider::new(Default::default());
//!
//!     // Create discoverer (finds pytest tests)
//!     let discoverer = PytestDiscoverer::new(Default::default());
//!
//!     // Create reporter (console + JUnit XML)
//!     let reporter = MultiReporter::new()
//!         .with_reporter(ConsoleReporter::new(true))
//!         .with_reporter(JUnitReporter::new("test-results/junit.xml".into()));
//!
//!     // Run tests
//!     let orchestrator = Orchestrator::new(config, provider, discoverer, reporter);
//!     let result = orchestrator.run().await?;
//!
//!     std::process::exit(result.exit_code());
//! }
//! ```
//!
//! ## Configuration
//!
//! Shotgun is configured via TOML files. See [`config`] module for schema details.
//!
//! ```toml
//! [shotgun]
//! max_parallel = 4
//! test_timeout_secs = 300
//! retry_count = 2
//!
//! [provider]
//! type = "docker"
//! image = "python:3.11"
//! volumes = [".:/app"]
//! working_dir = "/app"
//!
//! [discovery]
//! type = "pytest"
//! paths = ["tests"]
//!
//! [report]
//! output_dir = "test-results"
//! junit = true
//! ```
//!
//! ## Custom Providers
//!
//! You can implement custom providers for cloud platforms like Modal, AWS Lambda,
//! or Kubernetes by implementing the [`SandboxProvider`] and [`Sandbox`] traits,
//! or by using the [`provider::default::DefaultProvider`] with custom shell commands.
//!
//! [`SandboxProvider`]: provider::SandboxProvider
//! [`Sandbox`]: provider::Sandbox
//! [`TestDiscoverer`]: discovery::TestDiscoverer
//! [`Orchestrator`]: executor::Orchestrator

pub mod config;
pub mod connector;
pub mod discovery;
pub mod executor;
pub mod provider;
pub mod report;

// Re-export commonly used types for convenience.
// These are the types most users will need when setting up shotgun.

pub use config::{Config, load_config};
pub use discovery::{TestCase, TestDiscoverer, TestOutcome, TestResult};
pub use executor::{Orchestrator, RunResult};
pub use provider::{Sandbox, SandboxProvider};
pub use report::Reporter;
