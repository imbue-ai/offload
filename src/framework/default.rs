//! Default framework — custom shell commands for test discovery and execution.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord,
    discovery_error_detail,
};
use crate::config::DefaultFrameworkConfig;
use crate::provider::Command;
use crate::report::junit::TestsuiteXml;

/// Test framework using custom shell commands.
///
/// Provides maximum flexibility by delegating test discovery and execution
/// to user-defined shell commands. Suitable for any test framework.
///
/// # Configuration
///
/// See [`DefaultFrameworkConfig`] for available options including:
/// - `discover_command`: Shell command that outputs test IDs
/// - `run_command`: Command template with `{tests}` placeholder
/// - `result_file`: Optional JUnit XML path for detailed results
/// - `working_dir`: Directory for running commands
pub struct DefaultFramework {
    config: DefaultFrameworkConfig,
}

impl DefaultFramework {
    /// Creates a new default framework with the given configuration.
    pub fn new(config: DefaultFrameworkConfig) -> Self {
        Self { config }
    }

    /// Parse test discovery command output to extract test records.
    ///
    /// Expects one test ID per line. When `affinity_regex` is provided, each
    /// test's affinity key is derived from its ID: capture group 1 if the
    /// pattern has one, otherwise the whole match. A non-matching ID gets no key.
    fn parse_discover_output(
        &self,
        output: &str,
        group: &str,
        affinity_regex: Option<&regex::Regex>,
    ) -> Vec<TestRecord> {
        output
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut record = TestRecord::new(line, group);
                if let Some(key) = affinity_regex.and_then(|re| {
                    re.captures(line)
                        .and_then(|c| c.get(1).or_else(|| c.get(0)))
                        .map(|m| m.as_str())
                }) {
                    record = record.with_affinity_key(key);
                }
                record
            })
            .collect()
    }

    /// Compile the configured `affinity_key_regex`, if any.
    ///
    /// Returns `Ok(None)` when no pattern is configured. An invalid pattern
    /// surfaces as a [`FrameworkError::DiscoveryFailed`] naming the offending
    /// field.
    fn compile_affinity_regex(&self) -> FrameworkResult<Option<regex::Regex>> {
        self.config
            .affinity_key_regex
            .as_deref()
            .map(|pattern| {
                regex::Regex::new(pattern).map_err(|e| {
                    FrameworkError::DiscoveryFailed(format!(
                        "invalid affinity_key_regex '{}': {}",
                        pattern, e
                    ))
                })
            })
            .transpose()
    }

    /// Substitute {tests} and {result_file} placeholders in run command.
    ///
    /// Test IDs are shell-escaped to handle IDs containing spaces or special characters.
    fn substitute_command(&self, tests: &[TestInstance], result_path: &str) -> String {
        let test_ids: Vec<_> = tests
            .iter()
            .map(|t| shell_words::quote(t.id()).into_owned())
            .collect();
        self.config
            .run_command
            .replace("{tests}", &test_ids.join(" "))
            .replace("{result_file}", result_path)
    }
}

#[async_trait]
impl TestFramework for DefaultFramework {
    async fn discover(
        &self,
        _paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Compile the affinity key regex once per discovery (not per line).
        let affinity_regex = self.compile_affinity_regex()?;

        // Substitute {filters} placeholder with actual filters or empty string
        let discover_command = self.config.discover_command.replace("{filters}", filters);

        // Run test discovery command through shell to support pipes, globs, etc.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c");
        cmd.arg(&discover_command);

        if let Some(dir) = &self.config.working_dir {
            cmd.current_dir(dir);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        tracing::debug!("Discovery stdout:\n{}", stdout);
        if !stderr.is_empty() {
            tracing::debug!("Discovery stderr:\n{}", stderr);
        }

        if !output.status.success() {
            let detail = discovery_error_detail(&stderr, &stdout);
            let cmd_display = format!("sh -c '{}'", discover_command);
            return Err(FrameworkError::DiscoveryFailed(format!(
                "discover_command failed ({}):\n  command: {}\n  {}",
                output.status, cmd_display, detail
            )));
        }

        let tests = self.parse_discover_output(&stdout, group, affinity_regex.as_ref());

        if tests.is_empty() {
            tracing::warn!(
                "No tests discovered. Output: {}",
                discovery_error_detail(&stderr, &stdout)
            );
        }

        Ok(tests)
    }

    fn produce_test_execution_command(
        &self,
        tests: &[TestInstance],
        result_path: &str,
        _fail_fast: bool,
    ) -> Command {
        let full_command = self.substitute_command(tests, result_path);

        // Run through shell to properly handle quoted arguments, pipes, redirects, etc.
        // This matches the behavior of discover() and avoids issues with split_whitespace()
        // breaking commands like: jest "test with spaces" --reporter="json"
        let mut cmd = Command::new("sh").arg("-c").arg(&full_command);

        if let Some(dir) = &self.config.working_dir {
            cmd = cmd.working_dir(dir.to_string_lossy());
        }

        cmd
    }

    fn resolve_test_ids(
        &self,
        testsuites: &mut [TestsuiteXml],
        batch_test_ids: &[String],
    ) -> FrameworkResult<()> {
        let batch_set: HashSet<&str> = batch_test_ids.iter().map(|s| s.as_str()).collect();
        for testsuite in testsuites.iter_mut() {
            for testcase in &mut testsuite.testcases {
                let canonical = crate::config::format_test_id(
                    &self.config.test_id_format,
                    &testcase.name,
                    testcase.classname.as_deref(),
                );
                if batch_set.contains(canonical.as_str()) {
                    testcase.name = canonical;
                    testcase.classname = None;
                } else {
                    // Fall back to suffix matching when exact format doesn't match
                    match super::resolve_test_id_suffix_matching(
                        &testcase.name,
                        testcase.classname.as_deref(),
                        batch_test_ids,
                    ) {
                        Ok(resolved) => {
                            testcase.name = resolved.to_string();
                            testcase.classname = None;
                        }
                        Err(msg) => {
                            return Err(FrameworkError::Other(anyhow::anyhow!(
                                "JUnit testcase '{}' (format '{}' produced '{}') not resolved: {}",
                                testcase.name,
                                self.config.test_id_format,
                                canonical,
                                msg
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framework_with_regex(affinity_key_regex: Option<&str>) -> DefaultFramework {
        DefaultFramework::new(DefaultFrameworkConfig {
            discover_command: "echo".to_string(),
            run_command: "run {tests}".to_string(),
            result_file: None,
            working_dir: None,
            test_id_format: "{name}".to_string(),
            affinity_key_regex: affinity_key_regex.map(str::to_string),
            affinity_overhead_secs: 0.0,
        })
    }

    #[test]
    fn test_compile_affinity_regex_rejects_invalid_pattern() {
        let framework = framework_with_regex(Some("(unterminated"));
        let err = framework.compile_affinity_regex().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("affinity_key_regex"),
            "error should name the field: {msg}"
        );
    }

    #[test]
    fn test_parse_discover_output_capture_group_is_key() -> Result<(), Box<dyn std::error::Error>> {
        let framework = framework_with_regex(Some("^(.*?)::"));
        let re = framework.compile_affinity_regex()?;
        let records = framework.parse_discover_output(
            "tests/test_foo.py::test_a\ntests/test_foo.py::test_b",
            "grp",
            re.as_ref(),
        );
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].affinity_key(), Some("tests/test_foo.py"));
        assert_eq!(records[1].affinity_key(), Some("tests/test_foo.py"));
        Ok(())
    }

    #[test]
    fn test_parse_discover_output_whole_match_is_key() -> Result<(), Box<dyn std::error::Error>> {
        let framework = framework_with_regex(Some(r"^[^:]+"));
        let re = framework.compile_affinity_regex()?;
        let records =
            framework.parse_discover_output("tests/test_foo.py::test_a", "grp", re.as_ref());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].affinity_key(), Some("tests/test_foo.py"));
        Ok(())
    }

    #[test]
    fn test_parse_discover_output_non_matching_id_has_no_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let framework = framework_with_regex(Some("^(.*?)::"));
        let re = framework.compile_affinity_regex()?;
        let records = framework.parse_discover_output("no_separator_here", "grp", re.as_ref());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].affinity_key(), None);
        Ok(())
    }

    #[test]
    fn test_parse_discover_output_without_regex_has_no_key() {
        let framework = framework_with_regex(None);
        let records = framework.parse_discover_output("tests/test_foo.py::test_a", "grp", None);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].affinity_key(), None);
    }
}
