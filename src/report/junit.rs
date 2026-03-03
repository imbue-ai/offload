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
        // Parse all testsuites from the XML
        let parsed_testsuites = parse_all_testsuites_xml(xml_content);

        if parsed_testsuites.is_empty() {
            warn!(
                "Failed to parse JUnit XML ({} bytes), content preview: {:?}",
                xml_content.len(),
                &xml_content[..xml_content.len().min(200)]
            );
            return;
        }

        let before_count = self.test_outcomes.len();
        let mut total_testcases = 0;

        for testsuite in parsed_testsuites {
            let testcase_count = testsuite.testcases.len();
            total_testcases += testcase_count;

            // Update test outcomes from testcases
            for testcase in &testsuite.testcases {
                let test_id = TestId::new(testcase.classname.clone(), testcase.name.clone());
                let failed = testcase.failure.is_some() || testcase.error.is_some();
                self.update_test_outcome(test_id, failed);
            }

            self.testsuites.push(testsuite);
        }

        let after_count = self.test_outcomes.len();
        let new_tests = after_count - before_count;
        info!(
            "Added {} testsuites with {} testcases total, {} new unique tests (total: {})",
            self.testsuites.len(),
            total_testcases,
            new_tests,
            after_count
        );
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
/// Parse all testsuites from JUnit XML (handles multiple testsuites in one file).
fn parse_all_testsuites_xml(xml: &str) -> Vec<TestsuiteXml> {
    let mut reader = Reader::from_str(xml);
    let mut testsuites: Vec<TestsuiteXml> = Vec::new();
    let mut current_testsuite: Option<TestsuiteXml> = None;
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
                    // Start a new testsuite
                    current_testsuite = Some(TestsuiteXml {
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
                    // Self-closing testsuite (empty)
                    testsuites.push(TestsuiteXml {
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
                    if let Some(ref mut ts) = current_testsuite {
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
                b"testsuite" => {
                    // Complete current testsuite and add to list
                    if let Some(ts) = current_testsuite.take() {
                        testsuites.push(ts);
                    }
                }
                b"testcase" => {
                    if let Some(tc) = current_testcase.take()
                        && let Some(ref mut ts) = current_testsuite
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

    testsuites
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
        let mut parsed = parse_all_testsuites_xml(&content);
        testsuites.append(&mut parsed);
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
/// # Arguments
///
/// * `junit_path` - Path to the JUnit XML file
/// * `test_id_format` - Format string for constructing test IDs from JUnit attributes.
///   Uses placeholders `{name}` and `{classname}`.
///
/// # Returns
///
/// A HashMap where keys are test IDs and values are durations.
/// Returns an empty map if the file doesn't exist or can't be parsed.
pub fn load_test_durations(
    junit_path: &Path,
    test_id_format: &str,
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
                    let test_id = crate::config::format_test_id(
                        test_id_format,
                        &test_name,
                        classname.as_deref(),
                    );
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

    // Print to stdout so it's always visible
    eprintln!(
        "Loaded {} test durations from {} using format '{}'",
        durations.len(),
        junit_path.display(),
        test_id_format
    );

    // Show sample test IDs with durations to verify format matching
    if !durations.is_empty() {
        let mut samples: Vec<_> = durations.iter().collect();
        samples.sort_by(|a, b| b.1.cmp(a.1)); // Sort by duration descending
        eprintln!("Test durations loaded (sorted by duration):");
        for (test_id, duration) in samples.iter().take(10) {
            eprintln!("  {:.3}s  {}", duration.as_secs_f64(), test_id);
        }
        if samples.len() > 10 {
            eprintln!("  ... and {} more", samples.len() - 10);
        }
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

/// Loads test durations from a persistent timings cache file.
///
/// Each line has the format `<duration_ms> <test_id>`. Returns an empty map
/// if the file does not exist. Malformed lines are skipped with a warning.
pub fn load_timings(path: &Path) -> HashMap<String, std::time::Duration> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HashMap::new();
        }
        Err(e) => {
            warn!("Failed to read timings cache {}: {}", path.display(), e);
            return HashMap::new();
        }
    };

    let mut timings = HashMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some((ms_str, test_id_raw)) = trimmed.split_once(char::is_whitespace) else {
            warn!("Malformed timings line (no whitespace): {:?}", trimmed);
            continue;
        };

        let test_id = test_id_raw.trim();
        if test_id.is_empty() {
            warn!("Malformed timings line (empty test_id): {:?}", trimmed);
            continue;
        }

        let Ok(ms) = ms_str.parse::<u64>() else {
            warn!(
                "Malformed timings line (non-numeric ms {:?}): {:?}",
                ms_str, trimmed
            );
            continue;
        };

        timings.insert(test_id.to_string(), std::time::Duration::from_millis(ms));
    }

    info!("Loaded {} timings from {}", timings.len(), path.display());

    eprintln!(
        "Loaded {} test timings from {}",
        timings.len(),
        path.display()
    );

    if !timings.is_empty() {
        let mut samples: Vec<_> = timings.iter().collect();
        samples.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("Test timings loaded (sorted by duration):");
        for (test_id, duration) in samples.iter().take(10) {
            eprintln!("  {:.3}s  {}", duration.as_secs_f64(), test_id);
        }
        if samples.len() > 10 {
            eprintln!("  ... and {} more", samples.len() - 10);
        }
    }

    timings
}

/// Saves test durations to a persistent timings cache file.
///
/// Entries are sorted by test ID for deterministic output.
pub fn save_timings(
    path: &Path,
    timings: &HashMap<String, std::time::Duration>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut entries: Vec<_> = timings.iter().collect();
    entries.sort_by_key(|(id, _)| (*id).clone());

    let mut output = String::new();
    for (test_id, duration) in entries {
        output.push_str(&format!("{} {}\n", duration.as_millis(), test_id));
    }

    std::fs::write(path, output)
}

/// Merges new durations into existing timings using exponential moving average.
///
/// For existing keys: `updated = 0.2 * latest + 0.8 * current`
/// For new keys (not in `current`): inserted directly from `new`.
pub fn update_timings(
    current: &HashMap<String, std::time::Duration>,
    new: &HashMap<String, std::time::Duration>,
) -> HashMap<String, std::time::Duration> {
    let mut merged = current.clone();

    for (key, new_dur) in new {
        if let Some(cur_dur) = current.get(key) {
            let new_ms = new_dur.as_millis() as f64;
            let cur_ms = cur_dur.as_millis() as f64;
            let ema_ms = 0.2 * new_ms + 0.8 * cur_ms;
            merged.insert(
                key.clone(),
                std::time::Duration::from_millis(ema_ms.round() as u64),
            );
        } else {
            merged.insert(key.clone(), *new_dur);
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_parse_all_testsuites_xml() {
        let xml =
            r#"<testsuite name="test" tests="5" failures="1" errors="2" time="1.23"></testsuite>"#;
        let suites = parse_all_testsuites_xml(xml);
        assert_eq!(suites.len(), 1);
        let suite = &suites[0];
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

        let durations = load_test_durations(&path, "{name}");

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

        let durations = load_test_durations(&path, "{name}");

        assert_eq!(durations.len(), 1);
        // Should use max duration (3.0s, not 1.0s)
        assert_eq!(
            durations.get("test_flaky"),
            Some(&std::time::Duration::from_secs(3))
        );
    }

    #[test]
    fn test_load_test_durations_nonexistent_file() {
        let durations = load_test_durations(Path::new("/nonexistent/path/junit.xml"), "{name}");
        assert!(durations.is_empty());
    }

    #[test]
    fn test_load_timings_empty_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("timings");
        std::fs::write(&path, "").expect("write empty file");

        let timings = load_timings(&path);
        assert!(timings.is_empty());
    }

    #[test]
    fn test_load_timings_nonexistent() {
        let timings = load_timings(Path::new("/nonexistent/path/timings"));
        assert!(timings.is_empty());
    }

    #[test]
    fn test_save_and_load_timings_roundtrip() {
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("timings");

        let mut original = HashMap::new();
        original.insert(
            "tests/test_math.py::test_add".to_string(),
            Duration::from_millis(12340),
        );
        original.insert(
            "tests/test_math.py::test_sub".to_string(),
            Duration::from_millis(530),
        );
        original.insert(
            "tests/test_io.py::test_read".to_string(),
            Duration::from_millis(100),
        );

        save_timings(&path, &original).expect("save timings");
        let loaded = load_timings(&path);

        assert_eq!(loaded.len(), original.len());
        for (key, value) in &original {
            assert_eq!(loaded.get(key), Some(value), "mismatch for key {}", key);
        }
    }

    #[test]
    fn test_update_timings_ema_blending() {
        use std::time::Duration;

        let mut current = HashMap::new();
        current.insert("test_a".to_string(), Duration::from_millis(1000));
        let mut new = HashMap::new();
        new.insert("test_a".to_string(), Duration::from_millis(500));

        let merged = update_timings(&current, &new);
        // EMA: 0.2 * 500 + 0.8 * 1000 = 100 + 800 = 900
        assert_eq!(merged.get("test_a"), Some(&Duration::from_millis(900)));
    }

    #[test]
    fn test_update_timings_new_key() {
        use std::time::Duration;

        let current = HashMap::new();
        let mut new = HashMap::new();
        new.insert("test_new".to_string(), Duration::from_millis(500));

        let merged = update_timings(&current, &new);
        assert_eq!(merged.get("test_new"), Some(&Duration::from_millis(500)));
    }

    #[test]
    fn test_update_timings_preserves_untouched() {
        use std::time::Duration;

        let mut current = HashMap::new();
        current.insert("test_a".to_string(), Duration::from_millis(1000));
        current.insert("test_b".to_string(), Duration::from_millis(2000));
        let mut new = HashMap::new();
        new.insert("test_a".to_string(), Duration::from_millis(500));
        // test_b not in new — should be preserved unchanged

        let merged = update_timings(&current, &new);
        assert_eq!(merged.get("test_b"), Some(&Duration::from_millis(2000)));
    }
}
