//! JUnit XML merging utilities.
//!
//! This module provides functions to merge multiple JUnit XML files into one.
//! Used to combine results from parallel test execution across multiple sandboxes.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use tracing::{info, warn};

/// Tracks the outcome of a single test across multiple execution attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestStatus {
    Passed,
    Failed,
    Flaky, // Passed after failing
}

/// Unique identifier for a test case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TestId {
    classname: Option<String>,
    name: String,
}

impl TestId {
    fn new(classname: Option<String>, name: String) -> Self {
        Self { classname, name }
    }
}

/// Parsed testsuite element from JUnit XML.
#[derive(Debug, Clone)]
struct TestsuiteXml {
    name: String,
    tests: i32,
    failures: i32,
    errors: i32,
    skipped: i32,
    time: f64,
    timestamp: Option<String>,
    hostname: Option<String>,
    testcases: Vec<TestcaseXml>,
}

/// Parsed testcase element from JUnit XML.
#[derive(Debug, Clone)]
struct TestcaseXml {
    name: String,
    classname: Option<String>,
    time: f64,
    failure: Option<FailureXml>,
    error: Option<FailureXml>,
}

/// Parsed failure/error element from JUnit XML.
#[derive(Debug, Clone)]
struct FailureXml {
    message: Option<String>,
    content: String,
}

/// Accumulated JUnit results from all batches.
///
/// Thread-safe accumulator for JUnit XML content and test outcomes.
/// Used for early stopping (all tests passed) and final reporting.
#[derive(Debug, Default)]
pub struct MasterJunitReport {
    /// Parsed testsuites from each batch
    testsuites: Vec<TestsuiteXml>,
    /// Test outcomes by test ID (for deduplication and flaky detection)
    test_outcomes: HashMap<TestId, TestStatus>,
    /// Total tests expected (for early stopping)
    total_expected: usize,
}

impl MasterJunitReport {
    /// Creates a new master report expecting the given number of tests.
    pub fn new(total_expected: usize) -> Self {
        Self {
            testsuites: Vec::new(),
            test_outcomes: HashMap::new(),
            total_expected,
        }
    }

    /// Adds JUnit XML content from a batch.
    ///
    /// Parses the XML to extract test outcomes and accumulates the structured content.
    pub fn add_junit_xml(&mut self, xml_content: &str) {
        // Parse testsuite into structured form
        if let Some(testsuite) = parse_testsuite_xml(xml_content) {
            let testcase_count = testsuite.testcases.len();
            let before_count = self.test_outcomes.len();

            // Update test outcomes from testcases
            for testcase in &testsuite.testcases {
                let test_id = TestId::new(testcase.classname.clone(), testcase.name.clone());
                let failed = testcase.failure.is_some() || testcase.error.is_some();
                self.update_test_outcome(test_id, failed);
            }

            let after_count = self.test_outcomes.len();
            let new_tests = after_count - before_count;
            info!(
                "Added testsuite '{}': {} testcases parsed, {} new unique tests (total: {})",
                testsuite.name, testcase_count, new_tests, after_count
            );

            self.testsuites.push(testsuite);
        } else {
            warn!(
                "Failed to parse JUnit XML ({} bytes), content preview: {:?}",
                xml_content.len(),
                &xml_content[..xml_content.len().min(200)]
            );
        }
    }

    /// Updates the test outcome with flaky detection.
    fn update_test_outcome(&mut self, test_id: TestId, failed: bool) {
        let current = self.test_outcomes.get(&test_id).cloned();
        let new_status = match (current, failed) {
            (None, false) => TestStatus::Passed,
            (None, true) => TestStatus::Failed,
            (Some(TestStatus::Failed), false) => TestStatus::Flaky,
            (Some(TestStatus::Passed), true) => TestStatus::Flaky,
            (Some(status), _) => status, // Keep existing flaky or same status
        };
        self.test_outcomes.insert(test_id, new_status);
    }

    /// Returns the number of unique tests that have passed (including flaky).
    pub fn passed_count(&self) -> usize {
        self.test_outcomes
            .values()
            .filter(|s| **s == TestStatus::Passed || **s == TestStatus::Flaky)
            .count()
    }

    /// Returns the number of unique tests that failed (not flaky).
    pub fn failed_count(&self) -> usize {
        self.test_outcomes
            .values()
            .filter(|s| **s == TestStatus::Failed)
            .count()
    }

    /// Returns the number of flaky tests.
    pub fn flaky_count(&self) -> usize {
        self.test_outcomes
            .values()
            .filter(|s| **s == TestStatus::Flaky)
            .count()
    }

    /// Returns the total number of unique tests in the JUnit XML.
    pub fn total_count(&self) -> usize {
        self.test_outcomes.len()
    }

    /// Returns true if all expected tests have passed.
    pub fn all_passed(&self) -> bool {
        self.passed_count() >= self.total_expected
    }

    /// Writes the accumulated JUnit XML to a file using quick-xml Writer.
    pub fn write_to_file(&self, output_path: &Path) -> std::io::Result<()> {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Calculate totals from accumulated testsuites
        let total_tests: i32 = self.testsuites.iter().map(|s| s.tests).sum();
        let total_failures: i32 = self.testsuites.iter().map(|s| s.failures).sum();
        let total_errors: i32 = self.testsuites.iter().map(|s| s.errors).sum();
        let total_time: f64 = self.testsuites.iter().map(|s| s.time).sum();

        // Count actual testcases parsed
        let actual_testcases: usize = self.testsuites.iter().map(|s| s.testcases.len()).sum();
        let unique_tests = self.test_outcomes.len();

        info!(
            "Writing JUnit XML: {} testsuites, {} declared tests, {} actual testcases, {} unique outcomes",
            self.testsuites.len(),
            total_tests,
            actual_testcases,
            unique_tests
        );

        if total_tests as usize != actual_testcases {
            warn!(
                "Mismatch: XML declares {} tests but {} testcases were parsed",
                total_tests, actual_testcases
            );
        }

        let output = write_testsuites_xml(
            &self.testsuites,
            total_tests,
            total_failures,
            total_errors,
            total_time,
        );

        std::fs::write(output_path, output)?;
        info!("Wrote merged JUnit XML to {}", output_path.display());

        Ok(())
    }

    /// Returns summary counts: (passed, failed, flaky)
    pub fn summary(&self) -> (usize, usize, usize) {
        let passed = self
            .test_outcomes
            .values()
            .filter(|s| **s == TestStatus::Passed)
            .count();
        let failed = self.failed_count();
        let flaky = self.flaky_count();
        (passed, failed, flaky)
    }
}

/// Helper to extract a string attribute from a BytesStart element.
fn get_attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
}

/// Helper to extract an i32 attribute with default 0.
fn get_attr_i32(e: &BytesStart, name: &[u8]) -> i32 {
    get_attr(e, name).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Helper to extract an f64 attribute with default 0.0.
fn get_attr_f64(e: &BytesStart, name: &[u8]) -> f64 {
    get_attr(e, name)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

/// Parses a JUnit XML string into a TestsuiteXml structure using quick-xml.
fn parse_testsuite_xml(xml: &str) -> Option<TestsuiteXml> {
    let mut reader = Reader::from_str(xml);
    let mut testsuite: Option<TestsuiteXml> = None;
    let mut current_testcase: Option<TestcaseXml> = None;
    let mut current_failure_content = String::new();
    let mut in_failure = false;
    let mut in_error = false;
    let mut failure_message: Option<String> = None;
    let mut error_message: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"testsuite" => {
                    testsuite = Some(TestsuiteXml {
                        name: get_attr(&e, b"name").unwrap_or_default(),
                        tests: get_attr_i32(&e, b"tests"),
                        failures: get_attr_i32(&e, b"failures"),
                        errors: get_attr_i32(&e, b"errors"),
                        skipped: get_attr_i32(&e, b"skipped"),
                        time: get_attr_f64(&e, b"time"),
                        timestamp: get_attr(&e, b"timestamp"),
                        hostname: get_attr(&e, b"hostname"),
                        testcases: Vec::new(),
                    });
                }
                b"testcase" => {
                    current_testcase = Some(TestcaseXml {
                        name: get_attr(&e, b"name").unwrap_or_default(),
                        classname: get_attr(&e, b"classname"),
                        time: get_attr_f64(&e, b"time"),
                        failure: None,
                        error: None,
                    });
                }
                b"failure" => {
                    in_failure = true;
                    failure_message = get_attr(&e, b"message");
                    current_failure_content.clear();
                }
                b"error" => {
                    in_error = true;
                    error_message = get_attr(&e, b"message");
                    current_failure_content.clear();
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"testsuite" => {
                    testsuite = Some(TestsuiteXml {
                        name: get_attr(&e, b"name").unwrap_or_default(),
                        tests: get_attr_i32(&e, b"tests"),
                        failures: get_attr_i32(&e, b"failures"),
                        errors: get_attr_i32(&e, b"errors"),
                        skipped: get_attr_i32(&e, b"skipped"),
                        time: get_attr_f64(&e, b"time"),
                        timestamp: get_attr(&e, b"timestamp"),
                        hostname: get_attr(&e, b"hostname"),
                        testcases: Vec::new(),
                    });
                }
                b"testcase" => {
                    let tc = TestcaseXml {
                        name: get_attr(&e, b"name").unwrap_or_default(),
                        classname: get_attr(&e, b"classname"),
                        time: get_attr_f64(&e, b"time"),
                        failure: None,
                        error: None,
                    };
                    if let Some(ref mut ts) = testsuite {
                        ts.testcases.push(tc);
                    }
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if (in_failure || in_error)
                    && let Ok(text) = e.unescape()
                {
                    current_failure_content.push_str(&text);
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"testcase" => {
                    if let Some(tc) = current_testcase.take()
                        && let Some(ref mut ts) = testsuite
                    {
                        ts.testcases.push(tc);
                    }
                }
                b"failure" => {
                    if let Some(ref mut tc) = current_testcase {
                        tc.failure = Some(FailureXml {
                            message: failure_message.take(),
                            content: std::mem::take(&mut current_failure_content),
                        });
                    }
                    in_failure = false;
                }
                b"error" => {
                    if let Some(ref mut tc) = current_testcase {
                        tc.error = Some(FailureXml {
                            message: error_message.take(),
                            content: std::mem::take(&mut current_failure_content),
                        });
                    }
                    in_error = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    testsuite
}

/// Writes testsuites to XML string using quick-xml Writer.
fn write_testsuites_xml(
    testsuites: &[TestsuiteXml],
    total_tests: i32,
    total_failures: i32,
    total_errors: i32,
    total_time: f64,
) -> String {
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    // XML declaration
    let _ = writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));
    let _ = writer.write_event(Event::Text(BytesText::new("\n")));

    // Root <testsuites> element
    let mut testsuites_elem = BytesStart::new("testsuites");
    testsuites_elem.push_attribute(("name", "offload"));
    testsuites_elem.push_attribute(("tests", total_tests.to_string().as_str()));
    testsuites_elem.push_attribute(("failures", total_failures.to_string().as_str()));
    testsuites_elem.push_attribute(("errors", total_errors.to_string().as_str()));
    testsuites_elem.push_attribute(("time", format!("{:.3}", total_time).as_str()));
    let _ = writer.write_event(Event::Start(testsuites_elem));
    let _ = writer.write_event(Event::Text(BytesText::new("\n")));

    for suite in testsuites {
        write_testsuite(&mut writer, suite);
    }

    let _ = writer.write_event(Event::End(BytesEnd::new("testsuites")));
    let _ = writer.write_event(Event::Text(BytesText::new("\n")));

    String::from_utf8(writer.into_inner().into_inner()).unwrap_or_default()
}

/// Writes a single testsuite element to the XML writer.
fn write_testsuite(writer: &mut Writer<Cursor<Vec<u8>>>, suite: &TestsuiteXml) {
    let _ = writer.write_event(Event::Text(BytesText::new("  ")));

    let mut elem = BytesStart::new("testsuite");
    elem.push_attribute(("name", suite.name.as_str()));
    elem.push_attribute(("tests", suite.tests.to_string().as_str()));
    elem.push_attribute(("failures", suite.failures.to_string().as_str()));
    elem.push_attribute(("errors", suite.errors.to_string().as_str()));
    elem.push_attribute(("skipped", suite.skipped.to_string().as_str()));
    elem.push_attribute(("time", format!("{:.3}", suite.time).as_str()));
    if let Some(ref ts) = suite.timestamp {
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    if let Some(ref hn) = suite.hostname {
        elem.push_attribute(("hostname", hn.as_str()));
    }
    let _ = writer.write_event(Event::Start(elem));

    for tc in &suite.testcases {
        write_testcase(writer, tc);
    }

    let _ = writer.write_event(Event::End(BytesEnd::new("testsuite")));
    let _ = writer.write_event(Event::Text(BytesText::new("\n")));
}

/// Writes a single testcase element to the XML writer.
fn write_testcase(writer: &mut Writer<Cursor<Vec<u8>>>, tc: &TestcaseXml) {
    let mut elem = BytesStart::new("testcase");
    elem.push_attribute(("name", tc.name.as_str()));
    if let Some(ref cn) = tc.classname {
        elem.push_attribute(("classname", cn.as_str()));
    }
    elem.push_attribute(("time", format!("{:.3}", tc.time).as_str()));

    let has_content = tc.failure.is_some() || tc.error.is_some();

    if has_content {
        let _ = writer.write_event(Event::Start(elem));

        if let Some(ref failure) = tc.failure {
            write_failure_or_error(writer, "failure", failure);
        }
        if let Some(ref error) = tc.error {
            write_failure_or_error(writer, "error", error);
        }

        let _ = writer.write_event(Event::End(BytesEnd::new("testcase")));
    } else {
        let _ = writer.write_event(Event::Empty(elem));
    }
}

/// Writes a failure or error element.
fn write_failure_or_error(writer: &mut Writer<Cursor<Vec<u8>>>, tag: &str, failure: &FailureXml) {
    let mut elem = BytesStart::new(tag);
    if let Some(ref msg) = failure.message {
        elem.push_attribute(("message", msg.as_str()));
    }
    let _ = writer.write_event(Event::Start(elem));
    let _ = writer.write_event(Event::Text(BytesText::new(&failure.content)));
    let _ = writer.write_event(Event::End(BytesEnd::new(tag)));
}

/// Thread-safe handle to a MasterJunitReport.
pub type SharedJunitReport = Arc<Mutex<MasterJunitReport>>;

/// Merges multiple JUnit XML files into a single output file using quick-xml.
pub fn merge_junit_files(parts_dir: &Path, output_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut part_files: Vec<_> = std::fs::read_dir(parts_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "xml"))
        .map(|e| e.path())
        .collect();

    part_files.sort();

    if part_files.is_empty() {
        warn!("No JUnit XML files found in {}", parts_dir.display());
        let output = write_testsuites_xml(&[], 0, 0, 0, 0.0);
        std::fs::write(output_path, output)?;
        return Ok(());
    }

    info!(
        "Merging {} JUnit XML files from {}",
        part_files.len(),
        parts_dir.display()
    );

    let mut testsuites = Vec::new();

    for path in &part_files {
        let content = std::fs::read_to_string(path)?;
        if let Some(suite) = parse_testsuite_xml(&content) {
            testsuites.push(suite);
        }
    }

    // Calculate totals from parsed testsuites
    let total_tests: i32 = testsuites.iter().map(|s| s.tests).sum();
    let total_failures: i32 = testsuites.iter().map(|s| s.failures).sum();
    let total_errors: i32 = testsuites.iter().map(|s| s.errors).sum();
    let total_time: f64 = testsuites.iter().map(|s| s.time).sum();

    let output = write_testsuites_xml(
        &testsuites,
        total_tests,
        total_failures,
        total_errors,
        total_time,
    );
    std::fs::write(output_path, output)?;
    info!("Wrote merged JUnit XML to {}", output_path.display());

    Ok(())
}

/// Loads test durations from an existing JUnit XML file.
///
/// Parses the XML and extracts the duration (`time` attribute) for each test case.
/// If a test appears multiple times (e.g., from retries), the maximum duration is used.
///
/// The `format` parameter specifies how to convert JUnit's `classname` and `name`
/// attributes into the framework's native test ID format. This is necessary because
/// different test frameworks store test identity differently in JUnit XML.
///
/// # Arguments
///
/// * `junit_path` - Path to the JUnit XML file
/// * `format` - The JUnit format to use for converting test IDs
///
/// # Returns
///
/// A HashMap where keys are test IDs and values are durations.
/// Returns an empty map if the file doesn't exist or can't be parsed.
pub fn load_test_durations(
    junit_path: &Path,
    format: super::JunitFormat,
) -> HashMap<String, std::time::Duration> {
    let mut durations = HashMap::new();

    let content = match std::fs::read_to_string(junit_path) {
        Ok(c) => c,
        Err(e) => {
            info!(
                "Could not read JUnit XML for durations: {} ({})",
                junit_path.display(),
                e
            );
            return durations;
        }
    };

    let mut reader = Reader::from_str(&content);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.name().as_ref() == b"testcase" => {
                let name = get_attr(&e, b"name");
                let classname = get_attr(&e, b"classname");
                let time = get_attr_f64(&e, b"time");

                if let Some(test_name) = name {
                    let classname_str = classname.as_deref().unwrap_or("");
                    let test_id = format.to_test_id(classname_str, &test_name);
                    let duration = std::time::Duration::from_secs_f64(time);
                    // Use max duration if test appears multiple times (from retries)
                    durations
                        .entry(test_id)
                        .and_modify(|existing: &mut std::time::Duration| {
                            if duration > *existing {
                                *existing = duration;
                            }
                        })
                        .or_insert(duration);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    info!(
        "Loaded {} test durations from {}",
        durations.len(),
        junit_path.display()
    );

    // Debug: show a sample of loaded test IDs to help diagnose mismatches
    if !durations.is_empty() {
        let sample_ids: Vec<&String> = durations.keys().take(3).collect();
        tracing::debug!(
            "Sample loaded test IDs (format={:?}): {:?}",
            format,
            sample_ids
        );
    }

    durations
}

/// Removes the parts directory after merging.
pub fn cleanup_parts(parts_dir: &Path) -> std::io::Result<()> {
    if parts_dir.exists() {
        std::fs::remove_dir_all(parts_dir)?;
        info!("Cleaned up JUnit parts directory: {}", parts_dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::JunitFormat;

    #[test]
    fn test_parse_testcase_passed() {
        let xml = r#"<?xml version="1.0"?>
<testsuite name="test" tests="1" failures="0">
    <testcase classname="foo.bar" name="test_something" time="0.1" />
</testsuite>"#;

        let mut report = MasterJunitReport::new(1);
        report.add_junit_xml(xml);

        assert_eq!(report.total_count(), 1);
        assert_eq!(report.passed_count(), 1);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_parse_testcase_failed() {
        let xml = r#"<?xml version="1.0"?>
<testsuite name="test" tests="1" failures="1">
    <testcase classname="foo.bar" name="test_fail" time="0.1">
        <failure message="oops">stack trace</failure>
    </testcase>
</testsuite>"#;

        let mut report = MasterJunitReport::new(1);
        report.add_junit_xml(xml);

        assert_eq!(report.total_count(), 1);
        assert_eq!(report.passed_count(), 0);
        assert_eq!(report.failed_count(), 1);
    }

    #[test]
    fn test_flaky_detection() {
        let xml_fail = r#"<?xml version="1.0"?>
<testsuite name="test" tests="1" failures="1">
    <testcase classname="foo.bar" name="test_flaky" time="0.1">
        <failure message="oops">stack trace</failure>
    </testcase>
</testsuite>"#;

        let xml_pass = r#"<?xml version="1.0"?>
<testsuite name="test" tests="1" failures="0">
    <testcase classname="foo.bar" name="test_flaky" time="0.1" />
</testsuite>"#;

        let mut report = MasterJunitReport::new(1);
        report.add_junit_xml(xml_fail);
        assert_eq!(report.failed_count(), 1);

        report.add_junit_xml(xml_pass);
        assert_eq!(report.failed_count(), 0);
        assert_eq!(report.flaky_count(), 1);
        assert_eq!(report.passed_count(), 1); // flaky counts as passed
    }

    #[test]
    fn test_parse_testsuite_xml() {
        let xml =
            r#"<testsuite name="test" tests="5" failures="1" errors="2" time="1.23"></testsuite>"#;
        let suite = parse_testsuite_xml(xml).unwrap();
        assert_eq!(suite.name, "test");
        assert_eq!(suite.tests, 5);
        assert_eq!(suite.failures, 1);
        assert_eq!(suite.errors, 2);
        assert!((suite.time - 1.23).abs() < 0.001);
    }

    #[test]
    fn test_load_test_durations() {
        use std::io::Write;

        let xml = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="test">
    <testcase name="test_fast" classname="foo" time="0.5" />
    <testcase name="test_slow" classname="foo" time="10.0" />
    <testcase name="test_medium" classname="foo" time="2.5" />
  </testsuite>
</testsuites>"#;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("junit.xml");
        let mut file = std::fs::File::create(&path).expect("create file");
        file.write_all(xml.as_bytes()).expect("write xml");

        // Use default format (just returns name)
        let durations = load_test_durations(&path, JunitFormat::Default);

        assert_eq!(durations.len(), 3);
        assert_eq!(
            durations.get("test_fast"),
            Some(&std::time::Duration::from_millis(500))
        );
        assert_eq!(
            durations.get("test_slow"),
            Some(&std::time::Duration::from_secs(10))
        );
        assert_eq!(
            durations.get("test_medium"),
            Some(&std::time::Duration::from_millis(2500))
        );
    }

    #[test]
    fn test_load_test_durations_uses_max_for_duplicates() {
        use std::io::Write;

        // Same test appears multiple times (from retries) - should use max duration
        let xml = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="batch1">
    <testcase name="test_flaky" classname="foo" time="1.0" />
  </testsuite>
  <testsuite name="batch2">
    <testcase name="test_flaky" classname="foo" time="3.0" />
  </testsuite>
</testsuites>"#;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("junit.xml");
        let mut file = std::fs::File::create(&path).expect("create file");
        file.write_all(xml.as_bytes()).expect("write xml");

        // Use default format (just returns name)
        let durations = load_test_durations(&path, JunitFormat::Default);

        assert_eq!(durations.len(), 1);
        // Should use max duration (3.0s, not 1.0s)
        assert_eq!(
            durations.get("test_flaky"),
            Some(&std::time::Duration::from_secs(3))
        );
    }

    #[test]
    fn test_load_test_durations_nonexistent_file() {
        let durations = load_test_durations(
            Path::new("/nonexistent/path/junit.xml"),
            JunitFormat::Default,
        );
        assert!(durations.is_empty());
    }

    #[test]
    fn test_load_test_durations_with_pytest_format() {
        use std::io::Write;

        // Test that the pytest format produces correct test IDs
        let xml = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="test">
    <testcase name="test_foo" classname="libs.mng.api.test_list" time="1.5" />
    <testcase name="test_bar" classname="apps.changelings.cli.add_test" time="2.0" />
  </testsuite>
</testsuites>"#;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("junit.xml");
        let mut file = std::fs::File::create(&path).expect("create file");
        file.write_all(xml.as_bytes()).expect("write xml");

        // Use pytest format
        let durations = load_test_durations(&path, JunitFormat::Pytest);

        assert_eq!(durations.len(), 2);
        assert_eq!(
            durations.get("libs/mng/api/test_list.py::test_foo"),
            Some(&std::time::Duration::from_millis(1500))
        );
        assert_eq!(
            durations.get("apps/changelings/cli/add_test.py::test_bar"),
            Some(&std::time::Duration::from_secs(2))
        );
    }
}
