//! Vitest framework implementation using `vitest list --json` for discovery.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord};
use crate::config::VitestFrameworkConfig;
use crate::provider::Command;

/// Test framework for JavaScript/TypeScript vitest projects.
///
/// Uses `vitest list --json --includeTaskLocation` for test discovery and
/// generates commands with JUnit XML output for structured result parsing.
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

/// Source location from vitest's JSON output.
#[derive(Debug, serde::Deserialize)]
struct VitestLocation {
    line: u32,
    #[allow(dead_code)]
    column: u32,
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
            .arg("--reporter=junit")
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
        assert!(cmd.args.contains(&"--reporter=junit".to_string()));
        assert!(
            cmd.args
                .contains(&"--outputFile=/tmp/junit.xml".to_string())
        );
        assert!(cmd.args.contains(&"--no-coverage".to_string()));

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
