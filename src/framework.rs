//! Test framework traits and implementations.
//!
//! This module provides a framework-agnostic interface for collecting tests
//! and parsing their results. It supports pytest, cargo test, and custom
//! test frameworks via the [`TestFramework`] trait.
//!
//! # Architecture
//!
//! The framework system has three main responsibilities:
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
//! │  discover(&paths) ──────────► Vec<TestRecord>                   │
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
//! # Built-in Frameworks
//!
//! | Implementation | Target | Discovery Method |
//! |----------------|--------|------------------|
//! | [`pytest::PytestFramework`] | pytest | `pytest --collect-only -q` |
//! | [`cargo::CargoFramework`] | Rust | `cargo test --list` |
//! | [`default::DefaultFramework`] | Any | Custom shell commands |
//!
//! # Custom Frameworks
//!
//! Implement [`TestFramework`] to support new test frameworks:
//!
//! ```no_run
//! use async_trait::async_trait;
//! use offload::framework::*;
//! use offload::provider::{Command, ExecResult};
//! use std::path::PathBuf;
//!
//! struct MyFramework;
//!
//! #[async_trait]
//! impl TestFramework for MyFramework {
//!     async fn discover(&self, paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
//!         // Discover tests in the given paths
//!         todo!()
//!     }
//!
//!     fn produce_test_execution_command(&self, tests: &[TestInstance]) -> Command {
//!         // Generate command to run these tests
//!         todo!()
//!     }
//!
//!     fn parse_results(&self, output: &ExecResult, result_file: Option<&str>)
//!         -> FrameworkResult<Vec<TestResult>> {
//!         // Parse test results from output
//!         todo!()
//!     }
//! }
//! ```

pub mod cargo;
pub mod default;
pub mod pytest;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::{Command, ExecResult};

/// Result type for framework operations.
///
/// All framework methods return this type, wrapping either a success
/// value or a [`FrameworkError`].
pub type FrameworkResult<T> = Result<T, FrameworkError>;

/// Errors that can occur during test discovery and result parsing.
///
/// # Error Categories
///
/// - **Discovery**: Problems finding tests (command failed, no tests found)
/// - **Parsing**: Problems interpreting output (invalid format, encoding)
/// - **Execution**: Problems running the test discovery command
#[derive(Debug, thiserror::Error)]
pub enum FrameworkError {
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

    /// Failed to execute the test discovery or run command.
    #[error("Command execution failed: {0}")]
    ExecFailed(String),

    /// I/O error reading files or directories.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Other framework-related errors.
    #[error("Framework error: {0}")]
    Other(#[from] anyhow::Error),
}

/// A record of a test and its execution history.
///
/// `TestRecord` owns the test metadata and collects results from multiple
/// execution attempts. This enables flaky test detection and retry tracking.
///
/// # Thread Safety
///
/// Results are stored in a `Mutex` to allow concurrent updates from
/// multiple test executions.
///
/// # Example
///
/// ```
/// use offload::framework::TestRecord;
///
/// let record = TestRecord::new("tests/test_math.py::test_add");
/// let test = record.test();
/// // ... execute test in sandbox ...
/// // test.record_result(result);
/// ```
#[derive(Serialize, Deserialize)]
pub struct TestRecord {
    /// Unique identifier for this test.
    pub id: String,

    /// Human-readable display name.
    pub name: String,

    /// Source file where the test is defined.
    pub file: Option<PathBuf>,

    /// Line number where the test is defined.
    pub line: Option<u32>,

    /// Tags, markers, or labels associated with the test.
    pub markers: Vec<String>,

    /// Whether this test is known to be flaky.
    pub flaky: bool,

    /// Whether this test should be skipped.
    pub skipped: bool,

    /// Number of times to retry this test if it fails.
    /// Set per-test to allow group-specific retry counts.
    pub retry_count: usize,

    /// Group name this test belongs to (for JUnit testsuite grouping).
    pub group: Option<String>,

    /// Results from each execution attempt.
    /// Skipped during serialization as it contains runtime state.
    #[serde(skip)]
    results: Mutex<Vec<TestResult>>,

    /// Whether this test has been counted as passed (for early stopping).
    /// Used to avoid double-counting when multiple instances of the same test pass.
    #[serde(skip)]
    has_recorded_pass: AtomicBool,
}

impl TestRecord {
    /// Creates a new test record with the given ID.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        let name = id
            .split("::")
            .last()
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.clone());
        Self {
            id,
            name,
            file: None,
            line: None,
            markers: Vec::new(),
            flaky: false,
            skipped: false,
            retry_count: 0,
            group: None,
            results: Mutex::new(Vec::new()),
            has_recorded_pass: AtomicBool::new(false),
        }
    }

    /// Sets the retry count for this test.
    pub fn with_retry_count(mut self, count: usize) -> Self {
        self.retry_count = count;
        self
    }

    /// Sets the source file path.
    pub fn with_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Sets the source line number.
    pub fn with_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Adds a marker/tag to the test.
    pub fn with_marker(mut self, marker: impl Into<String>) -> Self {
        self.markers.push(marker.into());
        self
    }

    /// Marks this test as flaky.
    pub fn set_flaky(mut self) -> Self {
        self.flaky = true;
        self
    }

    /// Marks this test as skipped.
    pub fn set_skipped(mut self) -> Self {
        self.skipped = true;
        self
    }

    /// Sets the group name for this test (for JUnit testsuite grouping).
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Creates a `TestInstance` handle for execution in a sandbox.
    pub fn test(&self) -> TestInstance<'_> {
        TestInstance::new(self)
    }

    /// Records a result from a test execution.
    pub fn record_result(&self, result: TestResult) {
        if let Ok(mut guard) = self.results.lock() {
            guard.push(result);
        }
    }

    /// Returns the results collected so far.
    pub fn results(&self) -> Vec<TestResult> {
        self.results
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Returns whether this test is flaky based on recorded results.
    ///
    /// A test is flaky if it has both passes and failures across attempts.
    pub fn is_flaky(&self) -> bool {
        if let Ok(results) = self.results.lock() {
            let has_pass = results.iter().any(|r| r.outcome == TestOutcome::Passed);
            let has_fail = results
                .iter()
                .any(|r| r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error);
            has_pass && has_fail
        } else {
            false
        }
    }

    /// Returns whether this test passed.
    ///
    /// With parallel retries, returns true if ANY attempt passed.
    pub fn passed(&self) -> bool {
        self.results
            .lock()
            .ok()
            .map(|guard| guard.iter().any(|r| r.outcome == TestOutcome::Passed))
            .unwrap_or(false)
    }

    /// Atomically marks this test as having passed.
    ///
    /// Returns `true` if this was the first time marking the test as passed,
    /// `false` if it was already marked. Used for early stopping to count
    /// each test only once.
    pub fn try_mark_passed(&self) -> bool {
        // swap returns the previous value, so true means it was already set
        !self.has_recorded_pass.swap(true, Ordering::SeqCst)
    }

    /// Returns the number of execution attempts.
    pub fn attempt_count(&self) -> usize {
        self.results.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// Returns the final/canonical result (last result).
    /// Returns the final result for this test.
    ///
    /// If multiple attempts were run in parallel, returns passed if any passed.
    /// A test that passed after failures is marked as flaky.
    pub fn final_result(&self) -> Option<TestResult> {
        let results = self.results.lock().ok()?;
        if results.is_empty() {
            return None;
        }

        let any_passed = results.iter().any(|r| r.outcome == TestOutcome::Passed);
        let any_failed = results
            .iter()
            .any(|r| r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error);

        // Return passed result if any passed, otherwise first failure
        let mut result = if any_passed {
            results
                .iter()
                .find(|r| r.outcome == TestOutcome::Passed)
                .cloned()?
        } else {
            results.first().cloned()?
        };

        // Mark as flaky if passed but had failures
        if any_passed && any_failed {
            result.error_message = Some("Flaky - passed on parallel retry".to_string());
        }

        // Copy group from record to result
        result.group = self.group.clone();

        Some(result)
    }
}

impl std::fmt::Debug for TestRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let results_debug = self
            .results
            .lock()
            .as_ref()
            .map(|guard| format!("{:?}", &**guard))
            .unwrap_or_else(|_| "<poisoned>".to_string());
        f.debug_struct("TestRecord")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("file", &self.file)
            .field("line", &self.line)
            .field("markers", &self.markers)
            .field("flaky", &self.flaky)
            .field("skipped", &self.skipped)
            .field("results", &results_debug)
            .finish()
    }
}

/// A named group of tests with their execution results.
///
/// `TestGroup` owns the test records for a group and allows callers to
/// inspect results after execution completes. Results are stored in each
/// `TestRecord` via interior mutability.
///
/// # Example
///
/// ```
/// use offload::framework::{TestGroup, TestRecord};
///
/// let tests = vec![
///     TestRecord::new("test_one"),
///     TestRecord::new("test_two"),
/// ];
/// let group = TestGroup::new("my-group", tests);
///
/// assert_eq!(group.name(), "my-group");
/// assert_eq!(group.tests().len(), 2);
/// ```
#[derive(Debug)]
pub struct TestGroup {
    name: String,
    tests: Vec<TestRecord>,
}

impl TestGroup {
    /// Creates a new test group with the given name and tests.
    pub fn new(name: impl Into<String>, tests: Vec<TestRecord>) -> Self {
        Self {
            name: name.into(),
            tests,
        }
    }

    /// Returns the group name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns a reference to the tests in this group.
    pub fn tests(&self) -> &[TestRecord] {
        &self.tests
    }

    /// Returns the number of tests in this group.
    pub fn len(&self) -> usize {
        self.tests.len()
    }

    /// Returns true if this group has no tests.
    pub fn is_empty(&self) -> bool {
        self.tests.is_empty()
    }

    /// Returns the number of tests that passed.
    pub fn passed_count(&self) -> usize {
        self.tests.iter().filter(|t| t.passed()).count()
    }

    /// Returns the number of tests that failed.
    pub fn failed_count(&self) -> usize {
        self.tests
            .iter()
            .filter(|t| {
                t.final_result().is_some_and(|r| {
                    r.outcome == TestOutcome::Failed || r.outcome == TestOutcome::Error
                })
            })
            .count()
    }

    /// Returns the number of flaky tests (passed on retry).
    pub fn flaky_count(&self) -> usize {
        self.tests.iter().filter(|t| t.is_flaky()).count()
    }
}

/// A lightweight handle to a test for execution in a sandbox.
///
/// `TestInstance` holds a reference to a [`TestRecord`] and provides read access
/// to test metadata plus the ability to record results. Sandboxes only
/// see `TestInstance`, while `TestRecord` owns the data and aggregates results.
///
/// # Lifetime
///
/// The lifetime `'a` ties this `TestInstance` to its associated `TestRecord`.
#[derive(Debug, Clone, Copy)]
pub struct TestInstance<'a> {
    /// Reference to the test record containing all data.
    record: &'a TestRecord,
}

impl<'a> TestInstance<'a> {
    /// Creates a new test handle for the given record.
    pub fn new(record: &'a TestRecord) -> Self {
        Self { record }
    }

    /// Returns the unique identifier for this test.
    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// Returns the human-readable display name.
    pub fn name(&self) -> &str {
        &self.record.name
    }

    /// Returns the source file where the test is defined.
    pub fn file(&self) -> Option<&Path> {
        self.record.file.as_deref()
    }

    /// Returns the line number where the test is defined.
    pub fn line(&self) -> Option<u32> {
        self.record.line
    }

    /// Returns the tags/markers associated with the test.
    pub fn markers(&self) -> &[String] {
        &self.record.markers
    }

    /// Returns whether this test is known to be flaky.
    pub fn flaky(&self) -> bool {
        self.record.flaky
    }

    /// Returns whether this test should be skipped.
    pub fn skipped(&self) -> bool {
        self.record.skipped
    }

    /// Returns the test ID as an owned String.
    pub fn id_owned(&self) -> String {
        self.record.id.clone()
    }

    /// Returns the underlying test record.
    pub fn record(&self) -> &'a TestRecord {
        self.record
    }

    /// Records a result from executing this test.
    ///
    /// The result is stored in the associated `TestRecord`.
    pub fn record_result(&self, result: TestResult) {
        self.record.record_result(result);
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
    /// The ID of the test that was executed.
    pub test_id: String,

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

    /// Group name this test belongs to (for JUnit testsuite grouping).
    pub group: Option<String>,
}

impl TestResult {
    /// Creates a new test result for the given test ID.
    pub fn new(test_id: impl Into<String>, outcome: TestOutcome) -> Self {
        Self {
            test_id: test_id.into(),
            outcome,
            duration: std::time::Duration::ZERO,
            stdout: String::new(),
            stderr: String::new(),
            error_message: None,
            stack_trace: None,
            group: None,
        }
    }

    /// Sets the duration.
    pub fn with_duration(mut self, duration: std::time::Duration) -> Self {
        self.duration = duration;
        self
    }

    /// Sets the stdout.
    pub fn with_stdout(mut self, stdout: impl Into<String>) -> Self {
        self.stdout = stdout.into();
        self
    }

    /// Sets the stderr.
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.stderr = stderr.into();
        self
    }

    /// Sets the error message.
    pub fn with_error(mut self, message: impl Into<String>) -> Self {
        self.error_message = Some(message.into());
        self
    }

    /// Sets the stack trace.
    pub fn with_stack_trace(mut self, trace: impl Into<String>) -> Self {
        self.stack_trace = Some(trace.into());
        self
    }

    /// Sets the group name for this result.
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }
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
    /// use offload::framework::TestOutcome;
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

/// Trait for collecting tests and parsing their results.
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
/// - [`pytest::PytestFramework`] - Python pytest framework
/// - [`cargo::CargoFramework`] - Rust cargo test framework
/// - [`default::DefaultFramework`] - Custom shell-based framework
///
/// # Thread Safety
///
/// Frameworks must be `Send + Sync` to allow sharing across async tasks.
///
/// # Example Implementation
///
/// ```no_run
/// use async_trait::async_trait;
/// use offload::framework::*;
/// use offload::provider::{Command, ExecResult};
/// use std::path::PathBuf;
///
/// struct JestFramework { config_path: PathBuf }
///
/// #[async_trait]
/// impl TestFramework for JestFramework {
///     async fn discover(&self, paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>> {
///         // Run: jest --listTests
///         // Parse output to extract test files
///         todo!()
///     }
///
///     fn produce_test_execution_command(&self, tests: &[TestInstance]) -> Command {
///         let test_args: Vec<_> = tests.iter().map(|t| t.id()).collect();
///         Command::new("jest")
///             .args(test_args)
///             .arg("--ci")
///             .arg("--reporters=jest-junit")
///     }
///
///     fn parse_results(&self, output: &ExecResult, result_file: Option<&str>)
///         -> FrameworkResult<Vec<TestResult>> {
///         // Parse JUnit XML from jest-junit reporter
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait TestFramework: Send + Sync {
    /// Discovers tests in the given paths.
    ///
    /// This method typically runs a framework-specific discovery command
    /// (e.g., `pytest --collect-only`, `cargo test --list`) and parses
    /// the output to extract test records.
    ///
    /// # Arguments
    ///
    /// * `paths` - Directories or files to search for tests. If empty,
    ///   uses framework-default paths from configuration.
    ///
    /// # Returns
    ///
    /// A list of discovered [`TestRecord`] objects, or an error if discovery
    /// failed (command error, parse error, etc.).
    async fn discover(&self, paths: &[PathBuf]) -> FrameworkResult<Vec<TestRecord>>;

    /// Generates a command to run the specified tests.
    ///
    /// The returned [`Command`] should:
    /// - Run only the specified tests (not all tests)
    /// - Produce output that can be parsed by [`parse_results`](Self::parse_results)
    /// - Generate a result file if the framework supports it
    ///
    /// # Arguments
    ///
    /// * `tests` - Tests to execute (borrowed from TestRecords)
    ///
    /// # Example Output
    ///
    /// For pytest: `pytest -v tests/test_a.py::test_func tests/test_b.py::test_other`
    fn produce_test_execution_command(&self, tests: &[TestInstance]) -> Command;

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
    ) -> FrameworkResult<Vec<TestResult>>;
}
