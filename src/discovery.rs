//! Test discovery traits and implementations.
//!
//! This module provides a framework-agnostic interface for discovering tests
//! and parsing their results. It supports pytest, cargo test, and custom
//! test frameworks via the [`TestFramework`] trait.
//!
//! # Architecture
//!
//! The discovery system has three main responsibilities:
//!
//! 1. **Discover**: Find tests in the codebase ([`TestFramework::discover`])
//! 2. **Run**: Generate commands to execute tests ([`TestFramework::produce_test_execution_command`])
//! 3. **Parse**: Extract results from execution ([`TestFramework::parse_results`])
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     TestFramework                                │
//! ├─────────────────────────────────────────────────────────────────┤
//! │                                                                  │
//! │  discover(&paths) ──────────► Vec<TestCase>                     │
//! │                                    │                             │
//! │                                    ▼                             │
//! │  produce_test_execution_command(&tests) ──► Command             │
//! │                                    │                             │
//! │                                    ▼ (execute in sandbox)       │
//! │  parse_results(output) ────► Vec<TestResult>                    │
//! │                                                                  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Built-in Discoverers
//!
//! | Discoverer | Framework | Discovery Method |
//! |------------|-----------|------------------|
//! | [`pytest::PytestDiscoverer`] | pytest | `pytest --collect-only -q` |
//! | [`cargo::CargoDiscoverer`] | Rust | `cargo test --list` |
//! | [`default::DefaultDiscoverer`] | Any | Custom shell commands |
//!
//! # Custom Discoverers
//!
//! Implement [`TestFramework`] to support new test frameworks:
//!
//! ```no_run
//! use async_trait::async_trait;
//! use shotgun::discovery::*;
//! use shotgun::provider::{Command, ExecResult};
//! use std::path::PathBuf;
//!
//! struct MyDiscoverer;
//!
//! #[async_trait]
//! impl TestFramework for MyDiscoverer {
//!     async fn discover(&self, paths: &[PathBuf]) -> DiscoveryResult<Vec<TestCase>> {
//!         // Discover tests in the given paths
//!         todo!()
//!     }
//!
//!     fn produce_test_execution_command(&self, tests: &[TestCase]) -> Command {
//!         // Generate command to run these tests
//!         todo!()
//!     }
//!
//!     fn parse_results(&self, output: &ExecResult, result_file: Option<&str>)
//!         -> DiscoveryResult<Vec<TestResult>> {
//!         // Parse test results from output
//!         todo!()
//!     }
//! }
//! ```

pub mod cargo;
pub mod default;
pub mod pytest;

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::{Command, ExecResult};

/// Result type for discovery operations.
///
/// All discovery methods return this type, wrapping either a success
/// value or a [`DiscoveryError`].
pub type DiscoveryResult<T> = Result<T, DiscoveryError>;

/// Errors that can occur during test discovery and result parsing.
///
/// # Error Categories
///
/// - **Discovery**: Problems finding tests (command failed, no tests found)
/// - **Parsing**: Problems interpreting output (invalid format, encoding)
/// - **Execution**: Problems running the discovery command
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// Test discovery command failed or found no tests.
    ///
    /// Common causes: invalid path, framework not installed, syntax errors.
    #[error("Failed to discover tests: {0}")]
    DiscoveryFailed(String),

    /// Failed to parse test output or result files.
    ///
    /// Common causes: unexpected output format, invalid JUnit XML.
    #[error("Failed to parse test output: {0}")]
    ParseError(String),

    /// Failed to execute the discovery or test command.
    #[error("Command execution failed: {0}")]
    ExecFailed(String),

    /// I/O error reading files or directories.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Other discovery-related errors.
    #[error("Discovery error: {0}")]
    Other(#[from] anyhow::Error),
}

/// A single test case discovered in the codebase.
///
/// Test cases are identified by their unique `id` which typically includes
/// the file path and test name. The format depends on the test framework:
///
/// - **pytest**: `tests/test_math.py::TestClass::test_method`
/// - **cargo**: `tests::module::test_name`
/// - **default**: User-defined format
///
/// # Builder Pattern
///
/// Test cases can be constructed using the builder pattern:
///
/// ```
/// use shotgun::discovery::TestCase;
///
/// let test = TestCase::new("tests/test_math.py::test_add")
///     .with_file("tests/test_math.py")
///     .with_line(42)
///     .with_marker("slow")
///     .with_marker("integration");
/// ```
///
/// # Serialization
///
/// Test cases implement `Serialize` and `Deserialize` for caching discovered
/// tests or transmitting them between processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    /// Unique identifier for this test.
    ///
    /// This ID is used to select tests for execution and to correlate
    /// results back to the original test. Format is framework-specific.
    pub id: String,

    /// Human-readable display name.
    ///
    /// Typically the function/method name without the full path.
    /// Derived from `id` by default.
    pub name: String,

    /// Source file where the test is defined.
    ///
    /// Used for filtering and reporting. May be `None` if the framework
    /// doesn't provide file information.
    pub file: Option<PathBuf>,

    /// Line number where the test is defined.
    ///
    /// Enables IDE integration and precise error reporting.
    pub line: Option<u32>,

    /// Tags, markers, or labels associated with the test.
    ///
    /// Used for filtering (e.g., `@pytest.mark.slow`, `#[ignore]`).
    #[serde(default)]
    pub markers: Vec<String>,

    /// Whether this test is known to be flaky.
    ///
    /// Flaky tests may be handled differently (e.g., more retries,
    /// non-blocking failures).
    #[serde(default)]
    pub flaky: bool,

    /// Whether this test should be skipped.
    ///
    /// Skipped tests are reported but not executed.
    #[serde(default)]
    pub skipped: bool,
}

impl TestCase {
    /// Creates a new test case with the given ID.
    ///
    /// The display name is automatically derived from the ID by taking
    /// the last `::` separated component.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::discovery::TestCase;
    ///
    /// let test = TestCase::new("tests/math.py::TestCalc::test_add");
    /// assert_eq!(test.name, "test_add");
    /// ```
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

    /// Sets the source file path.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::discovery::TestCase;
    ///
    /// let test = TestCase::new("test_add").with_file("tests/test_math.py");
    /// ```
    pub fn with_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Sets the source line number.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::discovery::TestCase;
    ///
    /// let test = TestCase::new("test_add").with_line(42);
    /// ```
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Adds a marker/tag to the test.
    ///
    /// Can be called multiple times to add multiple markers.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::discovery::TestCase;
    ///
    /// let test = TestCase::new("test_db")
    ///     .with_marker("slow")
    ///     .with_marker("integration");
    /// ```
    pub fn with_marker(mut self, marker: impl Into<String>) -> Self {
        self.markers.push(marker.into());
        self
    }

    /// Marks this test as flaky.
    ///
    /// Flaky tests are tests that intermittently fail and may need
    /// special handling (more retries, quarantine, etc.).
    pub fn flaky(mut self) -> Self {
        self.flaky = true;
        self
    }

    /// Marks this test as skipped.
    ///
    /// Skipped tests will not be executed but will be reported.
    pub fn skipped(mut self) -> Self {
        self.skipped = true;
        self
    }
}

/// The result of executing a single test.
///
/// Contains the test outcome, timing, captured output, and any error details.
/// Test results are collected by the orchestrator and passed to reporters.
///
/// # Serialization
///
/// Results can be serialized for caching, logging, or transmission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// The test case that was executed.
    pub test: TestCase,

    /// The outcome of the test execution.
    pub outcome: TestOutcome,

    /// Wall-clock time the test took to execute.
    pub duration: std::time::Duration,

    /// Captured standard output from the test.
    ///
    /// May be empty if output capture is disabled or unsupported.
    pub stdout: String,

    /// Captured standard error from the test.
    pub stderr: String,

    /// Human-readable error message for failed tests.
    ///
    /// Typically the assertion message or exception description.
    pub error_message: Option<String>,

    /// Full stack trace for failed tests.
    ///
    /// Provides detailed debugging information for failures.
    pub stack_trace: Option<String>,
}

/// The outcome status of a test execution.
///
/// Tests can have four possible outcomes:
///
/// | Outcome | Description | Affects CI? |
/// |---------|-------------|-------------|
/// | Passed | Test assertions succeeded | No |
/// | Failed | Test assertions failed | Yes |
/// | Skipped | Test was not run (intentionally) | No |
/// | Error | Test crashed or setup failed | Yes |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestOutcome {
    /// Test passed successfully.
    ///
    /// All assertions succeeded and no errors occurred.
    Passed,

    /// Test failed due to an assertion.
    ///
    /// The test ran but an assertion/expectation was not met.
    Failed,

    /// Test was skipped and not executed.
    ///
    /// May be due to markers, conditions, or explicit skip calls.
    Skipped,

    /// Test errored during setup, execution, or teardown.
    ///
    /// Unlike `Failed`, this indicates the test couldn't complete
    /// normally (e.g., exception in fixtures, infrastructure failure).
    Error,
}

impl TestOutcome {
    /// Returns `true` if this outcome is considered successful.
    ///
    /// Both `Passed` and `Skipped` are considered successful outcomes
    /// that don't fail the test run.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::discovery::TestOutcome;
    ///
    /// assert!(TestOutcome::Passed.is_success());
    /// assert!(TestOutcome::Skipped.is_success());
    /// assert!(!TestOutcome::Failed.is_success());
    /// assert!(!TestOutcome::Error.is_success());
    /// ```
    pub fn is_success(&self) -> bool {
        matches!(self, TestOutcome::Passed | TestOutcome::Skipped)
    }
}

/// Trait for discovering tests and parsing their results.
///
/// A `TestFramework` encapsulates the logic for a specific test framework.
/// It handles three main operations:
///
/// 1. **Discovery**: Find tests in the codebase
/// 2. **Command generation**: Create commands to run specific tests
/// 3. **Result parsing**: Extract results from command output
///
/// # Implementors
///
/// - [`pytest::PytestDiscoverer`] - Python pytest framework
/// - [`cargo::CargoDiscoverer`] - Rust cargo test framework
/// - [`default::DefaultDiscoverer`] - Custom shell-based discovery
///
/// # Thread Safety
///
/// Discoverers must be `Send + Sync` to allow sharing across async tasks.
///
/// # Example Implementation
///
/// ```no_run
/// use async_trait::async_trait;
/// use shotgun::discovery::*;
/// use shotgun::provider::{Command, ExecResult};
/// use std::path::PathBuf;
///
/// struct JestDiscoverer { config_path: PathBuf }
///
/// #[async_trait]
/// impl TestFramework for JestDiscoverer {
///     async fn discover(&self, paths: &[PathBuf]) -> DiscoveryResult<Vec<TestCase>> {
///         // Run: jest --listTests
///         // Parse output to extract test files
///         todo!()
///     }
///
///     fn produce_test_execution_command(&self, tests: &[TestCase]) -> Command {
///         let test_args: Vec<_> = tests.iter().map(|t| t.id.as_str()).collect();
///         Command::new("jest")
///             .args(test_args)
///             .arg("--ci")
///             .arg("--reporters=jest-junit")
///     }
///
///     fn parse_results(&self, output: &ExecResult, result_file: Option<&str>)
///         -> DiscoveryResult<Vec<TestResult>> {
///         // Parse JUnit XML from jest-junit reporter
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait TestFramework: Send + Sync {
    /// Discovers tests in the given paths.
    ///
    /// This method typically runs a framework-specific collection command
    /// (e.g., `pytest --collect-only`, `cargo test --list`) and parses
    /// the output to extract test cases.
    ///
    /// # Arguments
    ///
    /// * `paths` - Directories or files to search for tests. If empty,
    ///   uses framework-default paths from configuration.
    ///
    /// # Returns
    ///
    /// A list of discovered [`TestCase`] objects, or an error if discovery
    /// failed (command error, parse error, etc.).
    async fn discover(&self, paths: &[PathBuf]) -> DiscoveryResult<Vec<TestCase>>;

    /// Generates a command to run the specified tests.
    ///
    /// The returned [`Command`] should:
    /// - Run only the specified tests (not all tests)
    /// - Produce output that can be parsed by [`parse_results`](Self::parse_results)
    /// - Generate a result file if the framework supports it
    ///
    /// # Arguments
    ///
    /// * `tests` - Test cases to execute (from previous discovery)
    ///
    /// # Example Output
    ///
    /// For pytest: `pytest -v tests/test_a.py::test_func tests/test_b.py::test_other`
    fn produce_test_execution_command(&self, tests: &[TestCase]) -> Command;

    /// Parses test results from command execution.
    ///
    /// This method extracts structured [`TestResult`]s from:
    /// - Command stdout/stderr
    /// - Result files (e.g., JUnit XML downloaded from sandbox)
    ///
    /// # Arguments
    ///
    /// * `output` - The execution result from running the test command
    /// * `result_file` - Optional contents of a result file (e.g., JUnit XML)
    ///
    /// # Returns
    ///
    /// A list of [`TestResult`] objects. If parsing fails completely,
    /// returns an error. If some results are missing, may return partial
    /// results based on what's available.
    fn parse_results(
        &self,
        output: &ExecResult,
        result_file: Option<&str>,
    ) -> DiscoveryResult<Vec<TestResult>>;
}
