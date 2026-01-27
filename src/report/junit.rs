//! JUnit XML report generation.
//!
//! Generates JUnit XML format test reports, which are the de facto standard
//! for CI/CD systems. The output is compatible with Jenkins, GitLab CI,
//! GitHub Actions, CircleCI, and other CI platforms.
//!
//! # Format
//!
//! The generated XML follows the JUnit schema:
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <testsuites tests="3" failures="1" errors="0" time="1.234">
//!   <testsuite name="shotgun" tests="3" failures="1" errors="0" skipped="0" time="1.234">
//!     <testcase classname="tests.test_math" name="test_add" time="0.100"/>
//!     <testcase classname="tests.test_math" name="test_sub" time="0.150">
//!       <failure message="AssertionError" type="AssertionError">
//!         assert 2 - 1 == 0
//!       </failure>
//!     </testcase>
//!     <testcase classname="tests.test_math" name="test_mul" time="0.050">
//!       <skipped/>
//!     </testcase>
//!   </testsuite>
//! </testsuites>
//! ```
//!
//! # Example
//!
//! ```
//! use shotgun::report::JUnitReporter;
//!
//! let reporter = JUnitReporter::new("test-results/junit.xml".into())
//!     .with_testsuite_name("my-project-tests");
//! ```

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use super::Reporter;
use crate::discovery::{TestCase, TestOutcome, TestResult};
use crate::executor::RunResult;

/// Reporter that generates JUnit XML test reports.
///
/// Collects test results during execution and writes a JUnit XML file
/// when the run completes. The file is created or overwritten at the
/// specified path.
///
/// # File Location
///
/// Parent directories are created automatically if they don't exist.
///
/// # Example
///
/// ```
/// use shotgun::report::JUnitReporter;
///
/// let reporter = JUnitReporter::new("build/test-results/junit.xml".into())
///     .with_testsuite_name("my-app");
/// ```
pub struct JUnitReporter {
    output_path: PathBuf,
    results: Mutex<Vec<TestResult>>,
    testsuite_name: String,
}

impl JUnitReporter {
    /// Creates a new JUnit reporter that writes to the given path.
    ///
    /// # Arguments
    ///
    /// * `output_path` - File path for the JUnit XML output
    pub fn new(output_path: PathBuf) -> Self {
        Self {
            output_path,
            results: Mutex::new(Vec::new()),
            testsuite_name: "shotgun".to_string(),
        }
    }

    /// Sets the test suite name in the XML output.
    ///
    /// The default name is `"shotgun"`. Set this to your project name
    /// for better identification in CI dashboards.
    ///
    /// # Arguments
    ///
    /// * `name` - Test suite name to use in the XML
    pub fn with_testsuite_name(mut self, name: impl Into<String>) -> Self {
        self.testsuite_name = name.into();
        self
    }

    /// Generate JUnit XML content from results.
    fn generate_xml(&self, run_result: &RunResult) -> anyhow::Result<String> {
        let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

        // XML declaration
        writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

        // Calculate totals
        let tests = run_result.results.len();
        let failures = run_result
            .results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Failed)
            .count();
        let errors = run_result
            .results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Error)
            .count();
        let skipped = run_result
            .results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Skipped)
            .count();
        let time = run_result.duration.as_secs_f64();

        // <testsuites>
        let mut testsuites = BytesStart::new("testsuites");
        testsuites.push_attribute(("tests", tests.to_string().as_str()));
        testsuites.push_attribute(("failures", failures.to_string().as_str()));
        testsuites.push_attribute(("errors", errors.to_string().as_str()));
        testsuites.push_attribute(("time", format!("{:.3}", time).as_str()));
        writer.write_event(Event::Start(testsuites))?;

        // <testsuite>
        let mut testsuite = BytesStart::new("testsuite");
        testsuite.push_attribute(("name", self.testsuite_name.as_str()));
        testsuite.push_attribute(("tests", tests.to_string().as_str()));
        testsuite.push_attribute(("failures", failures.to_string().as_str()));
        testsuite.push_attribute(("errors", errors.to_string().as_str()));
        testsuite.push_attribute(("skipped", skipped.to_string().as_str()));
        testsuite.push_attribute(("time", format!("{:.3}", time).as_str()));
        writer.write_event(Event::Start(testsuite))?;

        // Write each test case
        for result in &run_result.results {
            self.write_testcase(&mut writer, result)?;
        }

        // </testsuite>
        writer.write_event(Event::End(BytesEnd::new("testsuite")))?;

        // </testsuites>
        writer.write_event(Event::End(BytesEnd::new("testsuites")))?;

        let xml = String::from_utf8(writer.into_inner())?;
        Ok(xml)
    }

    /// Write a single test case element.
    fn write_testcase<W: std::io::Write>(
        &self,
        writer: &mut Writer<W>,
        result: &TestResult,
    ) -> anyhow::Result<()> {
        // Parse classname and name from test ID
        let (classname, name) = parse_test_id(&result.test.id);

        let mut testcase = BytesStart::new("testcase");
        testcase.push_attribute(("classname", classname.as_str()));
        testcase.push_attribute(("name", name.as_str()));
        testcase.push_attribute((
            "time",
            format!("{:.3}", result.duration.as_secs_f64()).as_str(),
        ));

        match result.outcome {
            TestOutcome::Passed => {
                // Self-closing tag for passed tests
                writer.write_event(Event::Empty(testcase))?;
            }
            TestOutcome::Failed => {
                writer.write_event(Event::Start(testcase))?;

                let mut failure = BytesStart::new("failure");
                if let Some(msg) = &result.error_message {
                    failure.push_attribute(("message", escape_xml(msg).as_str()));
                }
                failure.push_attribute(("type", "AssertionError"));
                writer.write_event(Event::Start(failure))?;

                // Write stack trace if available
                if let Some(trace) = &result.stack_trace {
                    writer.write_event(Event::Text(BytesText::new(&escape_xml(trace))))?;
                } else if !result.stderr.is_empty() {
                    writer.write_event(Event::Text(BytesText::new(&escape_xml(&result.stderr))))?;
                }

                writer.write_event(Event::End(BytesEnd::new("failure")))?;
                writer.write_event(Event::End(BytesEnd::new("testcase")))?;
            }
            TestOutcome::Error => {
                writer.write_event(Event::Start(testcase))?;

                let mut error = BytesStart::new("error");
                if let Some(msg) = &result.error_message {
                    error.push_attribute(("message", escape_xml(msg).as_str()));
                }
                error.push_attribute(("type", "Error"));
                writer.write_event(Event::Start(error))?;

                if let Some(trace) = &result.stack_trace {
                    writer.write_event(Event::Text(BytesText::new(&escape_xml(trace))))?;
                }

                writer.write_event(Event::End(BytesEnd::new("error")))?;
                writer.write_event(Event::End(BytesEnd::new("testcase")))?;
            }
            TestOutcome::Skipped => {
                writer.write_event(Event::Start(testcase))?;

                let skipped = BytesStart::new("skipped");
                writer.write_event(Event::Empty(skipped))?;

                writer.write_event(Event::End(BytesEnd::new("testcase")))?;
            }
        }

        // Write system-out and system-err if present
        // Note: For simplicity, we're not including these in the current output

        Ok(())
    }
}

#[async_trait]
impl Reporter for JUnitReporter {
    async fn on_discovery_complete(&self, _tests: &[TestCase]) {}

    async fn on_test_start(&self, _test: &TestCase) {}

    async fn on_test_complete(&self, result: &TestResult) {
        self.results.lock().unwrap().push(result.clone());
    }

    async fn on_run_complete(&self, result: &RunResult) {
        match self.generate_xml(result) {
            Ok(xml) => {
                // Ensure parent directory exists
                if let Some(parent) = self.output_path.parent()
                    && !parent.exists()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    tracing::error!("Failed to create output directory: {}", e);
                    return;
                }

                if let Err(e) = std::fs::write(&self.output_path, xml) {
                    tracing::error!("Failed to write JUnit XML: {}", e);
                } else {
                    tracing::info!("JUnit XML written to: {}", self.output_path.display());
                }
            }
            Err(e) => {
                tracing::error!("Failed to generate JUnit XML: {}", e);
            }
        }
    }
}

/// Parse a test ID into classname and name components.
fn parse_test_id(id: &str) -> (String, String) {
    // Handle common formats:
    // - tests/test_foo.py::TestClass::test_method
    // - tests::module::test_name
    // - test_foo::test_bar

    if let Some(idx) = id.rfind("::") {
        let classname = &id[..idx];
        let name = &id[idx + 2..];

        // Convert file path separators to dots for classname
        let classname = classname
            .replace("::", ".")
            .replace('/', ".")
            .replace(".py", "")
            .replace(".rs", "");

        (classname, name.to_string())
    } else {
        // No separator, use the whole thing as name
        ("unknown".to_string(), id.to_string())
    }
}

/// Escape special XML characters.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
        // Also remove invalid XML characters
        .chars()
        .filter(|c| matches!(c, '\t' | '\n' | '\r' | ' '..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_test_id_pytest() {
        let (classname, name) = parse_test_id("tests/test_foo.py::TestClass::test_method");
        assert_eq!(classname, "tests.test_foo.TestClass");
        assert_eq!(name, "test_method");
    }

    #[test]
    fn test_parse_test_id_rust() {
        let (classname, name) = parse_test_id("tests::module::test_name");
        assert_eq!(classname, "tests.module");
        assert_eq!(name, "test_name");
    }

    #[test]
    fn test_parse_test_id_simple() {
        let (classname, name) = parse_test_id("simple_test");
        assert_eq!(classname, "unknown");
        assert_eq!(name, "simple_test");
    }

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("<test>"), "&lt;test&gt;");
        assert_eq!(escape_xml("a & b"), "a &amp; b");
        assert_eq!(escape_xml("\"quoted\""), "&quot;quoted&quot;");
    }
}
