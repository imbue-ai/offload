//! Vitest framework implementation using `vitest list --json` for discovery.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord};
use crate::config::VitestFrameworkConfig;
use crate::provider::Command;

/// Test framework for JavaScript/TypeScript vitest projects.
///
/// Uses `vitest list --json` for test discovery and
/// JSON output with `--reporter=json` for results,
/// which are converted to JUnit XML via `xml_from_report`.
///
/// Test IDs use `{file} > {name}` format, e.g.
/// `tests/math.test.ts > math > add > adds two`.
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
    /// Test ID format: `{relative_file} > {name}`.
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

            let test_id = format!("{} > {}", relative, item.name);
            tests.push(TestRecord::new(&test_id, group));
        }

        Ok(tests)
    }
}

/// Check that no two test records share the same name part (everything after
/// the first ` > `). Vitest `--testNamePattern` matches by name alone, so
/// duplicates across files would cause over-selection.
fn check_unique_test_names(tests: &[TestRecord]) -> FrameworkResult<()> {
    use std::collections::HashMap;

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for t in tests {
        let name = t.id.split_once(" > ").map(|(_, n)| n).unwrap_or(&t.id);
        *seen.entry(name).or_insert(0) += 1;
    }

    let duplicates: Vec<&str> = seen
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(&name, _)| name)
        .collect();

    if !duplicates.is_empty() {
        let mut sorted = duplicates;
        sorted.sort_unstable();
        return Err(FrameworkError::DiscoveryFailed(format!(
            "Duplicate test names found across files: {}. \
             Vitest --testNamePattern cannot distinguish these.",
            sorted.join(", ")
        )));
    }

    Ok(())
}

/// A single test entry from vitest's JSON list output.
#[derive(Debug, serde::Deserialize)]
struct VitestListItem {
    name: String,
    file: String,
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
        cmd.arg("list").arg("--json");

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

        check_unique_test_names(&tests)?;

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

        // Split each test ID at the first ` > ` to get file and name parts.
        let mut selectors: Vec<&str> = Vec::new();
        let mut escaped_names: Vec<String> = Vec::new();
        for t in tests {
            if let Some((file, name)) = t.id().split_once(" > ") {
                selectors.push(file);
                escaped_names.push(regex::escape(name));
            } else {
                // Fallback: use the whole ID as a file selector only.
                selectors.push(t.id());
            }
        }

        selectors.sort_unstable();
        selectors.dedup();
        for selector in &selectors {
            cmd = cmd.arg(*selector);
        }

        escaped_names.sort_unstable();
        escaped_names.dedup();
        if !escaped_names.is_empty() {
            let pattern = format!("^({})$", escaped_names.join("|"));
            cmd = cmd.arg("--testNamePattern").arg(pattern);
        }

        cmd = cmd
            .arg("--reporter=json")
            .arg(format!("--outputFile={}", result_path));

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

                let classname = file.to_string();

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
            "tests/math.test.ts > math > add > adds two positive numbers",
            "grp",
        );
        let r2 = TestRecord::new(
            "tests/math.test.ts > math > subtract > subtracts two numbers",
            "grp",
        );
        let r3 = TestRecord::new("tests/string.test.ts > string utils > capitalize", "grp");
        let tests = vec![
            TestInstance::new(&r1),
            TestInstance::new(&r2),
            TestInstance::new(&r3),
        ];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml");

        assert_eq!(cmd.program, "npx");
        assert!(cmd.args.contains(&"vitest".to_string()));
        assert!(cmd.args.contains(&"run".to_string()));
        // File-only selectors (deduped: two tests from math.test.ts → one selector)
        assert!(cmd.args.contains(&"tests/math.test.ts".to_string()));
        assert!(cmd.args.contains(&"tests/string.test.ts".to_string()));
        assert!(cmd.args.contains(&"--reporter=json".to_string()));
        assert!(
            cmd.args
                .contains(&"--outputFile=/tmp/junit.xml".to_string())
        );
        assert!(!cmd.args.contains(&"--includeTaskLocation".to_string()));
        assert!(cmd.args.contains(&"--no-coverage".to_string()));

        // --testNamePattern with escaped names
        assert!(cmd.args.contains(&"--testNamePattern".to_string()));
        let tnp_idx = cmd
            .args
            .iter()
            .position(|a| a == "--testNamePattern")
            .unwrap();
        let pattern = &cmd.args[tnp_idx + 1];
        assert!(pattern.starts_with("^("));
        assert!(pattern.ends_with(")$"));
        assert!(pattern.contains("math > add > adds two positive numbers"));
        assert!(pattern.contains("math > subtract > subtracts two numbers"));
        assert!(pattern.contains("string utils > capitalize"));
        // Names are separated by |
        assert!(pattern.contains('|'));

        // --testNamePattern comes before --reporter=json
        let reporter_idx = cmd
            .args
            .iter()
            .position(|a| a == "--reporter=json")
            .unwrap();
        assert!(tnp_idx < reporter_idx);

        Ok(())
    }

    #[test]
    fn test_execution_command_escapes_regex_in_names() -> Result<(), Box<dyn std::error::Error>> {
        let config = VitestFrameworkConfig {
            command: "npx vitest".to_string(),
            ..Default::default()
        };
        let fw = VitestFramework::new(config)?;

        let r1 = TestRecord::new("tests/a.test.ts > suite (group) > test.name+thing*", "grp");
        let tests = vec![TestInstance::new(&r1)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/out.json");

        let tnp_idx = cmd
            .args
            .iter()
            .position(|a| a == "--testNamePattern")
            .unwrap();
        let pattern = &cmd.args[tnp_idx + 1];
        // Parentheses, dot, plus, star should be escaped
        assert!(
            pattern.contains(r"\("),
            "( should be escaped. Got: {}",
            pattern
        );
        assert!(
            pattern.contains(r"\)"),
            ") should be escaped. Got: {}",
            pattern
        );
        assert!(
            pattern.contains(r"\."),
            ". should be escaped. Got: {}",
            pattern
        );
        assert!(
            pattern.contains(r"\+"),
            "+ should be escaped. Got: {}",
            pattern
        );
        assert!(
            pattern.contains(r"\*"),
            "* should be escaped. Got: {}",
            pattern
        );

        Ok(())
    }

    #[test]
    fn test_discover_rejects_duplicate_names() -> Result<(), Box<dyn std::error::Error>> {
        let tests = vec![
            TestRecord::new("a.test.ts > suite > duplicate name", "grp"),
            TestRecord::new("b.test.ts > suite > duplicate name", "grp"),
            TestRecord::new("a.test.ts > suite > unique name", "grp"),
        ];

        let result = check_unique_test_names(&tests);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("suite > duplicate name"),
            "Error should mention the duplicate. Got: {}",
            msg
        );

        // No duplicates should pass.
        let unique_tests = vec![
            TestRecord::new("a.test.ts > name one", "grp"),
            TestRecord::new("b.test.ts > name two", "grp"),
        ];
        assert!(check_unique_test_names(&unique_tests).is_ok());

        Ok(())
    }

    #[test]
    fn test_xml_from_report_converts_json() -> Result<(), Box<dyn std::error::Error>> {
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
                        "failureMessages": []
                    },
                    {
                        "ancestorTitles": ["math", "add"],
                        "title": "adds negative",
                        "status": "failed",
                        "duration": 2.0,
                        "failureMessages": ["Expected 3 but got 2"]
                    }
                ]
            }]
        }"#;

        let junit = fw.xml_from_report(json)?;

        assert!(
            junit.contains("math.test.ts\""),
            "classname should be file path. Got: {}",
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
                        "failureMessages": []
                    },
                    {
                        "ancestorTitles": ["suite"],
                        "title": "is pending",
                        "status": "pending",
                        "duration": 0.0,
                        "failureMessages": []
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
                        "failureMessages": []
                    },
                    {
                        "ancestorTitles": ["suite"],
                        "title": "is skipped",
                        "status": "skipped",
                        "duration": 0.0,
                        "failureMessages": []
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
                "file": "/project/tests/math.test.ts"
            },
            {
                "name": "string utils > capitalize",
                "file": "/project/tests/string.test.ts"
            }
        ]"#;

        let cwd = std::path::Path::new("/project");
        let tests = fw.parse_discovery_json(json, cwd, "default")?;

        assert_eq!(tests.len(), 2);

        assert_eq!(
            tests[0].id,
            "tests/math.test.ts > math > add > adds two positive numbers"
        );
        assert_eq!(tests[0].group, "default");

        assert_eq!(
            tests[1].id,
            "tests/string.test.ts > string utils > capitalize"
        );

        Ok(())
    }
}
