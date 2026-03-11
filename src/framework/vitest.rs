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
/// which are converted to JUnit XML via `process_results`.
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

            let mut record = TestRecord::new(&test_id, group);
            record.name = item.name;
            record.file = Some(file_path);
            record.line = item.location.as_ref().map(|loc| loc.line);

            tests.push(record);
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

/// Intermediate structure for building JUnit XML testsuites.
struct JunitTestSuite {
    name: String,
    tests: i32,
    failures: i32,
    time: f64,
    testcases: Vec<JunitTestCase>,
}

/// Intermediate structure for building JUnit XML testcases.
struct JunitTestCase {
    classname: String,
    name: String,
    time: f64,
    failure_message: Option<String>,
    skipped: bool,
}

/// Escape special XML characters.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

    fn process_results(&self, raw_output: &str) -> String {
        // Parse vitest JSON output and convert to JUnit XML with line numbers
        // in classname for unique test identification.
        let report: VitestJsonReport = match serde_json::from_str(raw_output) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to parse vitest JSON output: {}", e);
                return raw_output.to_string();
            }
        };

        let cwd_str = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");

        // Collect all testsuites
        let mut suites: Vec<JunitTestSuite> = Vec::new();
        let mut total_tests = 0;
        let mut total_failures = 0;
        let mut total_time = 0.0;

        for test_result in &report.test_results {
            // Make file path relative
            let file = test_result
                .name
                .strip_prefix(&cwd_str)
                .unwrap_or(&test_result.name)
                .trim_start_matches('/');

            let mut suite_tests = 0;
            let mut suite_failures = 0;
            let mut suite_time = 0.0;
            let mut testcases = Vec::new();

            for ar in &test_result.assertion_results {
                // Skip pending/todo tests
                if ar.status == "pending" || ar.status == "todo" {
                    continue;
                }

                let name = ar
                    .ancestor_titles
                    .iter()
                    .chain(std::iter::once(&ar.title))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" > ");

                let line = ar.location.as_ref().map(|l| l.line);
                let classname = match line {
                    Some(l) => format!("{}:{}", file, l),
                    None => file.to_string(),
                };

                let duration_secs = ar.duration.unwrap_or(0.0) / 1000.0;
                let failed = ar.status == "failed";

                suite_tests += 1;
                suite_time += duration_secs;
                if failed {
                    suite_failures += 1;
                }

                testcases.push(JunitTestCase {
                    classname,
                    name,
                    time: duration_secs,
                    failure_message: if failed {
                        ar.failure_messages.first().cloned()
                    } else {
                        None
                    },
                    skipped: ar.status == "skipped",
                });
            }

            total_tests += suite_tests;
            total_failures += suite_failures;
            total_time += suite_time;

            suites.push(JunitTestSuite {
                name: file.to_string(),
                tests: suite_tests,
                failures: suite_failures,
                time: suite_time,
                testcases,
            });
        }

        // Build XML
        xml.push_str(&format!(
            "<testsuites name=\"vitest tests\" tests=\"{}\" failures=\"{}\" errors=\"0\" time=\"{:.6}\">\n",
            total_tests, total_failures, total_time
        ));

        for suite in &suites {
            if suite.testcases.is_empty() {
                continue;
            }
            xml.push_str(&format!(
                "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"0\" skipped=\"0\" time=\"{:.6}\">\n",
                xml_escape(&suite.name), suite.tests, suite.failures, suite.time
            ));
            for tc in &suite.testcases {
                if tc.skipped {
                    continue;
                }
                xml.push_str(&format!(
                    "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.6}\"",
                    xml_escape(&tc.classname),
                    xml_escape(&tc.name),
                    tc.time
                ));
                if let Some(ref msg) = tc.failure_message {
                    xml.push_str(&format!(
                        ">\n      <failure message=\"{}\">{}</failure>\n    </testcase>\n",
                        xml_escape(msg.lines().next().unwrap_or("")),
                        xml_escape(msg)
                    ));
                } else {
                    xml.push_str("/>\n");
                }
            }
            xml.push_str("  </testsuite>\n");
        }
        xml.push_str("</testsuites>\n");

        xml
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
    fn test_process_results_converts_json_with_lines() -> Result<(), Box<dyn std::error::Error>> {
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

        let junit = fw.process_results(json);

        // The file path in classname depends on CWD stripping; just check
        // that the line number suffix is present and the structure is correct.
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
    fn test_process_results_skips_pending() -> Result<(), Box<dyn std::error::Error>> {
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

        let junit = fw.process_results(json);

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
    fn test_process_results_skips_skipped() -> Result<(), Box<dyn std::error::Error>> {
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

        let junit = fw.process_results(json);

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
    fn test_process_results_passthrough_xml() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig::default();
        let fw = VitestFramework::new(config)?;

        // If input looks like XML (not JSON), it should attempt JSON parse, fail, and return as-is
        let xml = r#"<?xml version="1.0"?><testsuites><testsuite name="s" tests="1" failures="0" errors="0" skipped="0" time="0"><testcase name="t" classname="c" time="0"/></testsuite></testsuites>"#;
        let result = fw.process_results(xml);
        assert_eq!(result, xml);

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
        assert_eq!(tests[0].name, "math > add > adds two positive numbers");
        assert_eq!(
            tests[0]
                .file
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            Some("/project/tests/math.test.ts".to_string())
        );
        assert_eq!(tests[0].line, Some(6));
        assert_eq!(tests[0].group, "default");

        assert_eq!(
            tests[1].id,
            "tests/string.test.ts:10 > string utils > capitalize"
        );

        Ok(())
    }
}
