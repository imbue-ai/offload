//! Test discovery traits and implementations.
//!
//! This module provides framework-agnostic test discovery that can work
//! with pytest, cargo test, or any custom test framework.

pub mod cargo;
pub mod generic;
pub mod pytest;

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::{Command, ExecResult};

/// Result type for discovery operations.
pub type DiscoveryResult<T> = Result<T, DiscoveryError>;

/// Errors that can occur during test discovery.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("Failed to discover tests: {0}")]
    DiscoveryFailed(String),

    #[error("Failed to parse test output: {0}")]
    ParseError(String),

    #[error("Command execution failed: {0}")]
    ExecFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Discovery error: {0}")]
    Other(#[from] anyhow::Error),
}

/// Represents a single test case discovered by a test framework.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    /// Unique identifier for this test (e.g., "tests/test_foo.py::test_bar").
    pub id: String,

    /// Display name for the test.
    pub name: String,

    /// File path where the test is defined (if known).
    pub file: Option<PathBuf>,

    /// Line number where the test is defined (if known).
    pub line: Option<u32>,

    /// Test markers/tags/attributes.
    #[serde(default)]
    pub markers: Vec<String>,

    /// Whether this test is known to be flaky.
    #[serde(default)]
    pub flaky: bool,

    /// Whether this test is marked as skipped.
    #[serde(default)]
    pub skipped: bool,
}

impl TestCase {
    /// Create a new test case with the given ID.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        let name = id.split("::").last().unwrap_or(&id).to_string();
        Self {
            id,
            name,
            file: None,
            line: None,
            markers: Vec::new(),
            flaky: false,
            skipped: false,
        }
    }

    /// Set the file path.
    pub fn with_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Set the line number.
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Add a marker.
    pub fn with_marker(mut self, marker: impl Into<String>) -> Self {
        self.markers.push(marker.into());
        self
    }

    /// Mark as flaky.
    pub fn flaky(mut self) -> Self {
        self.flaky = true;
        self
    }

    /// Mark as skipped.
    pub fn skipped(mut self) -> Self {
        self.skipped = true;
        self
    }
}

/// Result of a single test execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// The test case that was run.
    pub test: TestCase,

    /// Outcome of the test.
    pub outcome: TestOutcome,

    /// Test duration.
    pub duration: std::time::Duration,

    /// Standard output from the test.
    pub stdout: String,

    /// Standard error from the test.
    pub stderr: String,

    /// Error message (if failed).
    pub error_message: Option<String>,

    /// Stack trace (if failed).
    pub stack_trace: Option<String>,
}

/// Outcome of a test execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestOutcome {
    /// Test passed.
    Passed,
    /// Test failed.
    Failed,
    /// Test was skipped.
    Skipped,
    /// Test errored (setup/teardown failed).
    Error,
}

impl TestOutcome {
    /// Check if this is a passing outcome.
    pub fn is_success(&self) -> bool {
        matches!(self, TestOutcome::Passed | TestOutcome::Skipped)
    }
}

/// A test discoverer finds tests and generates commands to run them.
///
/// Different implementations support different test frameworks like
/// pytest, cargo test, or custom frameworks.
#[async_trait]
pub trait TestDiscoverer: Send + Sync {
    /// Discover tests in the given paths.
    ///
    /// This typically runs a test collection command (e.g., `pytest --collect-only`)
    /// and parses the output to find test cases.
    async fn discover(&self, paths: &[PathBuf]) -> DiscoveryResult<Vec<TestCase>>;

    /// Generate a command to run the specified tests.
    ///
    /// The command should run only the given tests and produce results
    /// that can be parsed by `parse_results`.
    fn run_command(&self, tests: &[TestCase]) -> Command;

    /// Parse test results from command execution.
    ///
    /// This extracts test results from the command output and/or
    /// result files (e.g., JUnit XML).
    fn parse_results(&self, output: &ExecResult, result_file: Option<&str>) -> DiscoveryResult<Vec<TestResult>>;

    /// Get the framework name (for logging and config).
    fn name(&self) -> &'static str;
}

/// A boxed, type-erased test discoverer.
pub type DynDiscoverer = Box<dyn TestDiscoverer>;
