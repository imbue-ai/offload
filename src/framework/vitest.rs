//! Vitest framework implementation using `vitest list --json` for discovery.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord};
use crate::config::VitestFrameworkConfig;
use crate::provider::Command;

/// Test framework for JavaScript/TypeScript vitest projects.
///
/// Uses `vitest list --json --includeTaskLocation` for test discovery and
/// JSON output with `--reporter=json --includeTaskLocation` for results,
/// which are converted to JUnit XML via `xml_from_report`.
pub struct VitestFramework {
    config: VitestFrameworkConfig,
    /// The program to invoke (first token of `command`).
    program: String,
    /// Additional arguments parsed from `command` (tokens after the program).
    prefix_args: Vec<String>,
}

impl VitestFramework {
    /// Creates a new vitest framework, validating the command at construction time.
    pub fn new(config: VitestFrameworkConfig) -> FrameworkResult<Self> {
        let mut parts = shell_words::split(&config.command).map_err(|e| {
            FrameworkError::DiscoveryFailed(format!(
                "Failed to parse command '{}': {}",
                config.command, e
            ))
        })?;

        if parts.is_empty() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "Command '{}' produced no tokens after parsing",
                config.command
            )));
        }

        let program = parts.remove(0);
        let prefix_args = parts;

        Ok(Self {
            config,
            program,
            prefix_args,
        })
    }

    /// Parse vitest JSON discovery output into test records.
    ///
    /// The `cwd` is used to make absolute file paths relative.
    fn parse_discovery_json(
        &self,
        json_str: &str,
        cwd: &std::path::Path,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        let items: Vec<VitestListItem> = serde_json::from_str(json_str).map_err(|e| {
            FrameworkError::ParseError(format!("Failed to parse vitest list JSON: {}", e))
        })?;

        let mut tests = Vec::with_capacity(items.len());
        for item in items {
            let file_path = PathBuf::from(&item.file);
            let relative = file_path
                .strip_prefix(cwd)
                .unwrap_or(&file_path)
                .to_string_lossy();

            // Bake file:line into the ID for execution targeting.
            // Example: "tests/math.test.ts:6 > math > add > adds two positive numbers"
            let line_suffix = item
                .location
                .as_ref()
                .map_or(String::new(), |loc| format!(":{}", loc.line));
            let test_id = format!("{}{} > {}", relative, line_suffix, item.name);

            tests.push(TestRecord::new(&test_id, group));
        }

        Ok(tests)
    }
}

/// A single test entry from vitest's JSON list output.
#[derive(Debug, serde::Deserialize)]
struct VitestListItem {
    name: String,
    file: String,
    #[serde(default)]
    location: Option<VitestLocation>,
}

/// Source location from vitest's JSON list output.
#[derive(Debug, serde::Deserialize)]
struct VitestLocation {
    line: u32,
    #[allow(dead_code)]
    column: u32,
}

/// Vitest JSON report structure (top level).
#[derive(Debug, serde::Deserialize)]
struct VitestJsonReport {
    #[serde(rename = "testResults")]
    test_results: Vec<VitestJsonTestResult>,
}

/// A single test file's results from vitest JSON reporter.
#[derive(Debug, serde::Deserialize)]
struct VitestJsonTestResult {
    /// Absolute path to the test file.
    name: String,
    #[serde(rename = "assertionResults")]
    assertion_results: Vec<VitestJsonAssertionResult>,
}

/// A single test assertion result from vitest JSON reporter.
#[derive(Debug, serde::Deserialize)]
struct VitestJsonAssertionResult {
    #[serde(rename = "ancestorTitles")]
    ancestor_titles: Vec<String>,
    title: String,
    status: String,
    duration: Option<f64>,
    #[serde(default, rename = "failureMessages")]
    failure_messages: Vec<String>,
    #[serde(default)]
    location: Option<VitestJsonLocation>,
}

/// Source location from vitest JSON reporter output.
#[derive(Debug, serde::Deserialize)]
struct VitestJsonLocation {
    line: u32,
}

#[async_trait]
impl TestFramework for VitestFramework {
    async fn discover(
        &self,
        _paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        let mut cmd = tokio::process::Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd.arg(arg);
        }
        cmd.arg("list").arg("--json").arg("--includeTaskLocation");

        if !filters.is_empty() {
            let args = shell_words::split(filters).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid filter string '{}': {}",
                    filters, e
                ))
            })?;
            for arg in args {
                cmd.arg(arg);
            }
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "vitest list failed (exit {}): {}",
                output.status, stderr
            )));
        }

        let cwd = std::env::current_dir().map_err(|e| {
            FrameworkError::DiscoveryFailed(format!("Failed to get current directory: {}", e))
        })?;

        let tests = self.parse_discovery_json(&stdout, &cwd, group)?;

        if tests.is_empty() {
            tracing::warn!(
                "No tests discovered. stdout: {}, stderr: {}",
                stdout,
                stderr
            );
        }

        Ok(tests)
    }

    fn produce_test_execution_command(&self, tests: &[TestInstance], result_path: &str) -> Command {
        let mut cmd = Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd = cmd.arg(arg);
        }

        cmd = cmd.arg("run");

        // Extract file:line selectors from test IDs.
        // Test ID format: "{file}:{line} > {name}" -- extract the "file:line" part.
        let mut selectors: Vec<&str> = tests
            .iter()
            .filter_map(|t| t.id().split(" > ").next())
            .collect();
        selectors.sort_unstable();
        selectors.dedup();

        for selector in selectors {
            cmd = cmd.arg(selector);
        }

        cmd = cmd
            .arg("--reporter=json")
            .arg(format!("--outputFile={}", result_path))
            .arg("--includeTaskLocation");

        if let Some(run_args) = &self.config.run_args {
            match shell_words::split(run_args) {
                Ok(args) => {
                    for arg in args {
                        cmd = cmd.arg(arg);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse run_args '{}': {}", run_args, e);
                }
            }
        }

        cmd
    }

    fn report_format(&self) -> &str {
        "json"
    }

    /// Vitest can discover duplicate test IDs from `describe.each` that the
    /// JUnit report will deduplicate, so early stopping would miscount.
    fn supports_early_stopping(&self) -> bool {
        false
    }

    fn xml_from_report(&self, raw_output: &str) -> super::FrameworkResult<String> {
        use quick_xml::Writer;
        use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
        use std::io::Cursor;

        let report: VitestJsonReport = serde_json::from_str(raw_output).map_err(|e| {
            super::FrameworkError::ParseError(format!("Failed to parse vitest JSON output: {}", e))
        })?;

        let cwd_str = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

        let _ = writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));

        // Collect suite data first to compute totals.
        struct SuiteData {
            name: String,
            tests: i32,
            failures: i32,
            time: f64,
            cases: Vec<CaseData>,
        }
        struct CaseData {
            classname: String,
            name: String,
            time: f64,
            failure_message: Option<String>,
        }

        let mut suites = Vec::new();
        let mut total_tests = 0;
        let mut total_failures = 0;
        let mut total_time = 0.0;

        for test_result in &report.test_results {
            let file = test_result
                .name
                .strip_prefix(&cwd_str)
                .unwrap_or(&test_result.name)
                .trim_start_matches('/');

            let mut suite = SuiteData {
                name: file.to_string(),
                tests: 0,
                failures: 0,
                time: 0.0,
                cases: Vec::new(),
            };

            for ar in &test_result.assertion_results {
                if ar.status == "pending" || ar.status == "todo" || ar.status == "skipped" {
                    continue;
                }

                let name = ar
                    .ancestor_titles
                    .iter()
                    .chain(std::iter::once(&ar.title))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" > ");

                let classname = match ar.location.as_ref().map(|l| l.line) {
                    Some(l) => format!("{}:{}", file, l),
                    None => file.to_string(),
                };

                let duration_secs = ar.duration.unwrap_or(0.0) / 1000.0;
                let failed = ar.status == "failed";

                suite.tests += 1;
                suite.time += duration_secs;
                if failed {
                    suite.failures += 1;
                }

                suite.cases.push(CaseData {
                    classname,
                    name,
                    time: duration_secs,
                    failure_message: if failed {
                        ar.failure_messages.first().cloned()
                    } else {
                        None
                    },
                });
            }

            total_tests += suite.tests;
            total_failures += suite.failures;
            total_time += suite.time;
            suites.push(suite);
        }

        // Write <testsuites>
        let mut ts_elem = BytesStart::new("testsuites");
        ts_elem.push_attribute(("name", "vitest tests"));
        ts_elem.push_attribute(("tests", total_tests.to_string().as_str()));
        ts_elem.push_attribute(("failures", total_failures.to_string().as_str()));
        ts_elem.push_attribute(("errors", "0"));
        ts_elem.push_attribute(("time", format!("{:.6}", total_time).as_str()));
        let _ = writer.write_event(Event::Start(ts_elem));

        for suite in &suites {
            if suite.cases.is_empty() {
                continue;
            }

            let mut s_elem = BytesStart::new("testsuite");
            s_elem.push_attribute(("name", suite.name.as_str()));
            s_elem.push_attribute(("tests", suite.tests.to_string().as_str()));
            s_elem.push_attribute(("failures", suite.failures.to_string().as_str()));
            s_elem.push_attribute(("errors", "0"));
            s_elem.push_attribute(("skipped", "0"));
            s_elem.push_attribute(("time", format!("{:.6}", suite.time).as_str()));
            let _ = writer.write_event(Event::Start(s_elem));

            for tc in &suite.cases {
                let mut tc_elem = BytesStart::new("testcase");
                tc_elem.push_attribute(("classname", tc.classname.as_str()));
                tc_elem.push_attribute(("name", tc.name.as_str()));
                tc_elem.push_attribute(("time", format!("{:.6}", tc.time).as_str()));

                if let Some(ref msg) = tc.failure_message {
                    let _ = writer.write_event(Event::Start(tc_elem));

                    let mut fail_elem = BytesStart::new("failure");
                    let first_line = msg.lines().next().unwrap_or("");
                    fail_elem.push_attribute(("message", first_line));
                    let _ = writer.write_event(Event::Start(fail_elem));
                    let _ = writer.write_event(Event::Text(BytesText::new(msg)));
                    let _ = writer.write_event(Event::End(BytesEnd::new("failure")));

                    let _ = writer.write_event(Event::End(BytesEnd::new("testcase")));
                } else {
                    let _ = writer.write_event(Event::Empty(tc_elem));
                }
            }

            let _ = writer.write_event(Event::End(BytesEnd::new("testsuite")));
        }

        let _ = writer.write_event(Event::End(BytesEnd::new("testsuites")));

        String::from_utf8(writer.into_inner().into_inner()).map_err(|e| {
            super::FrameworkError::ParseError(format!("Failed to encode JUnit XML as UTF-8: {}", e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VitestFrameworkConfig;

    #[test]
    fn test_command_prefix_with_command() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig {
            command: "npx vitest".to_string(),
            ..Default::default()
        };
        let fw = VitestFramework::new(config)?;
        assert_eq!(fw.program, "npx");
        assert_eq!(fw.prefix_args, vec!["vitest"]);
        Ok(())
    }

    #[test]
    fn test_new_rejects_empty_command() {
        let config = VitestFrameworkConfig {
            command: "".to_string(),
            ..Default::default()
        };
        assert!(VitestFramework::new(config).is_err());
    }

    #[test]
    fn test_execution_command_builds_file_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig {
            command: "npx vitest".to_string(),
            run_args: Some("--no-coverage".to_string()),
            ..Default::default()
        };
        let fw = VitestFramework::new(config)?;

        let r1 = TestRecord::new(
            "tests/math.test.ts:6 > math > add > adds two positive numbers",
            "grp",
        );
        let r2 = TestRecord::new(
            "tests/math.test.ts:20 > math > subtract > subtracts two numbers",
            "grp",
        );
        let r3 = TestRecord::new("tests/string.test.ts:14 > string utils > capitalize", "grp");
        let tests = vec![
            TestInstance::new(&r1),
            TestInstance::new(&r2),
            TestInstance::new(&r3),
        ];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml");

        assert_eq!(cmd.program, "npx");
        assert!(cmd.args.contains(&"vitest".to_string()));
        assert!(cmd.args.contains(&"run".to_string()));
        assert!(cmd.args.contains(&"tests/math.test.ts:6".to_string()));
        assert!(cmd.args.contains(&"tests/math.test.ts:20".to_string()));
        assert!(cmd.args.contains(&"tests/string.test.ts:14".to_string()));
        assert!(cmd.args.contains(&"--reporter=json".to_string()));
        assert!(
            cmd.args
                .contains(&"--outputFile=/tmp/junit.xml".to_string())
        );
        assert!(cmd.args.contains(&"--includeTaskLocation".to_string()));
        assert!(cmd.args.contains(&"--no-coverage".to_string()));

        Ok(())
    }

    #[test]
    fn test_early_stopping_disabled() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;
        assert!(!fw.supports_early_stopping());
        Ok(())
    }

    #[test]
    fn test_xml_from_report_converts_json_with_lines() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;

        let json = r#"{
            "testResults": [{
                "name": "/project/tests/math.test.ts",
                "assertionResults": [
                    {
                        "ancestorTitles": ["math", "add"],
                        "title": "adds two numbers",
                        "status": "passed",
                        "duration": 1.5,
                        "failureMessages": [],
                        "location": {"line": 6}
                    },
                    {
                        "ancestorTitles": ["math", "add"],
                        "title": "adds negative",
                        "status": "failed",
                        "duration": 2.0,
                        "failureMessages": ["Expected 3 but got 2"],
                        "location": {"line": 10}
                    }
                ]
            }]
        }"#;

        let junit = fw.xml_from_report(json)?;

        assert!(
            junit.contains("math.test.ts:6\""),
            "classname should have :line for first test. Got: {}",
            junit
        );
        assert!(
            junit.contains("math.test.ts:10\""),
            "classname should have :line for second test. Got: {}",
            junit
        );
        assert!(
            junit.contains("math &gt; add &gt; adds two numbers"),
            "name should use > separator. Got: {}",
            junit
        );
        assert!(junit.contains("<failure"), "should have failure element");

        Ok(())
    }

    #[test]
    fn test_xml_from_report_skips_pending() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;

        let json = r#"{
            "testResults": [{
                "name": "/project/tests/a.test.ts",
                "assertionResults": [
                    {
                        "ancestorTitles": ["suite"],
                        "title": "runs",
                        "status": "passed",
                        "duration": 1.0,
                        "failureMessages": [],
                        "location": {"line": 5}
                    },
                    {
                        "ancestorTitles": ["suite"],
                        "title": "is pending",
                        "status": "pending",
                        "duration": 0.0,
                        "failureMessages": [],
                        "location": {"line": 10}
                    }
                ]
            }]
        }"#;

        let junit = fw.xml_from_report(json)?;

        assert!(
            junit.contains("runs"),
            "passed test should be in output. Got: {}",
            junit
        );
        assert!(
            !junit.contains("is pending"),
            "pending test should not be in output. Got: {}",
            junit
        );

        Ok(())
    }

    #[test]
    fn test_xml_from_report_skips_skipped() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;

        let json = r#"{
            "testResults": [{
                "name": "/project/tests/a.test.ts",
                "assertionResults": [
                    {
                        "ancestorTitles": ["suite"],
                        "title": "runs",
                        "status": "passed",
                        "duration": 1.0,
                        "failureMessages": [],
                        "location": {"line": 5}
                    },
                    {
                        "ancestorTitles": ["suite"],
                        "title": "is skipped",
                        "status": "skipped",
                        "duration": 0.0,
                        "failureMessages": [],
                        "location": {"line": 10}
                    }
                ]
            }]
        }"#;

        let junit = fw.xml_from_report(json)?;

        assert!(
            junit.contains("runs"),
            "passed test should be in output. Got: {}",
            junit
        );
        assert!(
            !junit.contains("is skipped"),
            "skipped test should not be in output. Got: {}",
            junit
        );

        Ok(())
    }

    #[test]
    fn test_xml_from_report_rejects_invalid_input() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;

        let result = fw.xml_from_report("not json");
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn test_parse_discovery_json() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;

        let json = r#"[
            {
                "name": "math > add > adds two positive numbers",
                "file": "/project/tests/math.test.ts",
                "location": { "line": 6, "column": 5 }
            },
            {
                "name": "string utils > capitalize",
                "file": "/project/tests/string.test.ts",
                "location": { "line": 10, "column": 5 }
            }
        ]"#;

        let cwd = std::path::Path::new("/project");
        let tests = fw.parse_discovery_json(json, cwd, "default")?;

        assert_eq!(tests.len(), 2);

        assert_eq!(
            tests[0].id,
            "tests/math.test.ts:6 > math > add > adds two positive numbers"
        );
        assert_eq!(tests[0].group, "default");

        assert_eq!(
            tests[1].id,
            "tests/string.test.ts:10 > string utils > capitalize"
        );

        Ok(())
    }
}
