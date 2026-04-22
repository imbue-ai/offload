//! Test framework traits and implementations for discovery, execution, and result parsing.
pub mod cargo;
pub mod default;
pub mod pytest;
pub mod vitest;

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::provider::Command;
use crate::report::junit::TestsuiteXml;

/// Build a human-readable detail string from captured process output.
/// Prefers stderr; falls back to stdout (capped at 500 chars); then "no output captured".
pub(crate) fn discovery_error_detail(stderr: &str, stdout: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        if stdout.len() > 500 {
            return format!("{}... (truncated)", &stdout[..500]);
        }
        return format!("(stderr empty, stdout): {}", stdout);
    }
    "no output captured".to_string()
}

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

/// A record of a test and its metadata.
#[derive(Debug, Serialize, Deserialize)]
pub struct TestRecord {
    /// Unique identifier for this test.
    pub id: String,

    /// Human-readable display name.
    pub name: String,

    /// Source file where the test is defined.
    pub file: Option<PathBuf>,

    /// Number of times to retry this test if it fails.
    /// Set per-test to allow group-specific retry counts.
    pub retry_count: usize,

    /// Group name this test belongs to (for JUnit testsuite grouping).
    pub group: String,

    /// Whether this test should be individually scheduled (one-per-batch).
    pub schedule_individual: bool,
}

impl TestRecord {
    /// Creates a new test record with the given ID and group.
    pub fn new(id: impl Into<String>, group: impl Into<String>) -> Self {
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
            retry_count: 0,
            group: group.into(),
            schedule_individual: false,
        }
    }

    /// Sets the retry count for this test.
    pub fn with_retry_count(mut self, count: usize) -> Self {
        self.retry_count = count;
        self
    }

    /// Marks this test for individual scheduling (one-per-batch).
    pub fn with_schedule_individual(mut self, schedule_individual: bool) -> Self {
        self.schedule_individual = schedule_individual;
        self
    }

    /// Sets the source file path.
    pub fn with_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Creates a `TestInstance` handle for execution in a sandbox.
    pub fn test(&self) -> TestInstance {
        TestInstance {
            id: self.id.clone(),
            group: self.group.clone(),
            schedule_individual: self.schedule_individual,
        }
    }
}

/// A lightweight handle to a test for execution in a sandbox.
#[derive(Debug, Clone)]
pub struct TestInstance {
    id: String,
    group: String,
    schedule_individual: bool,
}

impl TestInstance {
    pub fn new(record: &TestRecord) -> Self {
        Self {
            id: record.id.clone(),
            group: record.group.clone(),
            schedule_individual: record.schedule_individual,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn group(&self) -> &str {
        &self.group
    }

    pub fn schedule_individual(&self) -> bool {
        self.schedule_individual
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
    pub group: String,
}

impl TestResult {
    /// Creates a new test result for the given test ID and group.
    pub fn new(test_id: impl Into<String>, outcome: TestOutcome, group: impl Into<String>) -> Self {
        Self {
            test_id: test_id.into(),
            outcome,
            duration: std::time::Duration::ZERO,
            stdout: String::new(),
            stderr: String::new(),
            error_message: None,
            stack_trace: None,
            group: group.into(),
        }
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

/// Normalizes a test ID into a dotted format for suffix comparison.
///
/// Converts both pytest nodeids (`tests/test_foo.py::TestClass::test_method`)
/// and JUnit classname::name pairs into the same canonical dotted format.
pub(crate) fn normalize_test_id(s: &str) -> String {
    s.replace('/', ".").replace(".py", "").replace("::", ".")
}

/// Counts matching characters from the end of two strings.
pub(crate) fn longest_common_suffix_len(a: &str, b: &str) -> usize {
    a.chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(ac, bc)| ac == bc)
        .count()
}

/// Resolves a JUnit testcase to a batch test ID via suffix matching.
///
/// Reconstructs a test ID from the JUnit `name` and optional `classname`,
/// normalizes both sides, then finds the batch ID sharing the longest
/// common suffix. Returns `Ok(original_batch_id)` on a unique match,
/// or `Err(message)` if there is no match or an ambiguous tie.
pub(crate) fn resolve_test_id_suffix_matching(
    name: &str,
    classname: Option<&str>,
    batch_ids: &[String],
) -> Result<String, String> {
    let reconstructed = match classname {
        Some(cn) if !cn.is_empty() => format!("{}::{}", cn, name),
        _ => name.to_string(),
    };
    let norm_reconstructed = normalize_test_id(&reconstructed);

    let mut best_len: usize = 0;
    let mut best_ids: Vec<&String> = Vec::new();

    for batch_id in batch_ids {
        let norm_batch = normalize_test_id(batch_id);
        let suffix_len = longest_common_suffix_len(&norm_reconstructed, &norm_batch);
        match suffix_len.cmp(&best_len) {
            std::cmp::Ordering::Greater => {
                best_len = suffix_len;
                best_ids.clear();
                best_ids.push(batch_id);
            }
            std::cmp::Ordering::Equal => {
                best_ids.push(batch_id);
            }
            std::cmp::Ordering::Less => {}
        }
    }

    if best_len == 0 || best_ids.is_empty() {
        return Err(format!(
            "no batch ID matched JUnit testcase '{}' (normalized: '{}')",
            reconstructed, norm_reconstructed
        ));
    }

    match best_ids.len() {
        1 => Ok(best_ids[0].clone()),
        _ => {
            let tied: Vec<&str> = best_ids.iter().map(|s| s.as_str()).collect();
            Err(format!(
                "ambiguous match for JUnit testcase '{}' (normalized: '{}') — tied batch IDs: {:?}",
                reconstructed, norm_reconstructed, tied
            ))
        }
    }
}

/// Trait for collecting tests and generating execution commands.
///
/// A `TestFramework` encapsulates the logic for a specific test framework.
/// It handles two main operations:
///
/// 1. **Discovery**: Find tests in the codebase
/// 2. **Command generation**: Create commands to run specific tests
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
    /// * `filters` - Optional filter expression to narrow down test discovery.
    ///   The interpretation of this filter is framework-specific (e.g., test
    ///   name patterns, marker expressions).
    /// * `group` - Group name to assign to each discovered test record.
    ///
    /// # Returns
    ///
    /// A list of discovered [`TestRecord`] objects, or an error if discovery
    /// failed (command error, parse error, etc.).
    async fn discover(
        &self,
        paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>>;

    /// Generates a command to run the specified tests.
    ///
    /// The returned [`Command`] should:
    /// - Run only the specified tests (not all tests)
    /// - Produce structured output (e.g., JUnit XML) for result collection
    /// - Generate a result file if the framework supports it
    ///
    /// # Arguments
    ///
    /// * `tests` - Tests to execute (borrowed from TestRecords)
    fn produce_test_execution_command(
        &self,
        tests: &[TestInstance],
        result_path: &str,
        fail_fast: bool,
    ) -> Command;

    /// File format for the test result file produced by the framework.
    ///
    /// Used as the file extension for the result file path.
    /// Default: `"xml"` (JUnit XML). Frameworks that produce other formats
    /// (e.g., JSON) should override this.
    fn report_format(&self) -> &str {
        "xml"
    }

    /// Processes raw test result output into JUnit XML.
    ///
    /// Frameworks can override this to convert non-JUnit output formats
    /// (e.g., vitest JSON) into JUnit XML, or to filter artifacts from
    /// their JUnit output.
    ///
    /// Default implementation returns the input unchanged (assumes JUnit XML).
    fn xml_from_report(&self, raw_output: &str) -> FrameworkResult<String> {
        Ok(raw_output.to_string())
    }

    /// Resolves JUnit testcase names to canonical test IDs.
    ///
    /// Rewrites each testcase's `name` to the canonical test ID (matching
    /// what `discover` returned) and sets `classname` to `None`.
    fn resolve_test_ids(
        &self,
        testsuites: &mut [TestsuiteXml],
        batch_test_ids: &[String],
    ) -> FrameworkResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ids_to_records(ids: Vec<String>) -> Vec<TestRecord> {
        ids.into_iter()
            .map(|id| {
                let file = id.split("::").next().map(PathBuf::from);
                let mut record = TestRecord::new(id, "test-group");
                if let Some(f) = file {
                    record = record.with_file(f);
                }
                record
            })
            .collect()
    }

    #[test]
    fn test_discovery_error_detail_prefers_stderr() {
        let detail = discovery_error_detail("some error", "some output");
        assert_eq!(detail, "some error");
    }

    #[test]
    fn test_discovery_error_detail_falls_back_to_stdout() {
        let detail = discovery_error_detail("", "some output");
        assert_eq!(detail, "(stderr empty, stdout): some output");
    }

    #[test]
    fn test_discovery_error_detail_trims_whitespace() {
        let detail = discovery_error_detail("  \n  ", "  stdout content  \n");
        assert_eq!(detail, "(stderr empty, stdout): stdout content");
    }

    #[test]
    fn test_discovery_error_detail_no_output() {
        let detail = discovery_error_detail("", "");
        assert_eq!(detail, "no output captured");
    }

    #[test]
    fn test_discovery_error_detail_truncates_long_stdout() {
        let long_stdout = "x".repeat(600);
        let detail = discovery_error_detail("", &long_stdout);
        assert!(detail.contains("... (truncated)"));
        assert!(detail.len() < 600);
    }

    #[test]
    fn test_parse_test_id() {
        let records = test_ids_to_records(vec![
            "tests/test_math.py::test_addition".to_string(),
            "tests/test_math.py::TestClass::test_method".to_string(),
        ]);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "test_addition");
        assert_eq!(records[0].file, Some(PathBuf::from("tests/test_math.py")));
        assert_eq!(records[1].name, "test_method");
    }

    #[test]
    fn test_resolve_basic_suffix_match() {
        let batch_ids = vec!["tests/test_foo.py::test_bar".to_string()];
        let result =
            resolve_test_id_suffix_matching("test_bar", Some("tests.test_foo"), &batch_ids);
        assert_eq!(result, Ok("tests/test_foo.py::test_bar".to_string()));
    }

    #[test]
    fn test_resolve_with_class() {
        let batch_ids = vec![
            "tests/test_foo.py::ClassA::test_method".to_string(),
            "tests/test_foo.py::ClassB::test_method".to_string(),
        ];
        let result = resolve_test_id_suffix_matching(
            "test_method",
            Some("tests.test_foo.ClassA"),
            &batch_ids,
        );
        assert_eq!(
            result,
            Ok("tests/test_foo.py::ClassA::test_method".to_string())
        );
    }

    #[test]
    fn test_resolve_parametrized() {
        let batch_ids = vec!["tests/test_foo.py::test_bar[param1]".to_string()];
        let result =
            resolve_test_id_suffix_matching("test_bar[param1]", Some("tests.test_foo"), &batch_ids);
        assert_eq!(
            result,
            Ok("tests/test_foo.py::test_bar[param1]".to_string())
        );
    }

    #[test]
    fn test_resolve_ambiguous_error() {
        let batch_ids = vec![
            "a/test_foo.py::test_bar".to_string(),
            "b/test_foo.py::test_bar".to_string(),
        ];
        let result = resolve_test_id_suffix_matching("test_bar", Some("test_foo"), &batch_ids);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("ambiguous"),
            "expected 'ambiguous' in: {}",
            err
        );
    }

    #[test]
    fn test_resolve_no_match_error() {
        let batch_ids = vec!["tests/test_foo.py::test_bar".to_string()];
        let result =
            resolve_test_id_suffix_matching("test_unknown", Some("tests.test_other"), &batch_ids);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("no batch ID matched") || err.contains("ambiguous"),
            "unexpected error: {}",
            err
        );
    }
}
