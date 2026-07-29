//! Pytest framework implementation using `pytest --collect-only` for discovery.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord,
    discovery_error_detail,
};
use crate::config::PytestFrameworkConfig;
use crate::provider::Command;
use crate::report::junit::TestsuiteXml;

/// Test framework for Python pytest projects.
///
/// Uses `pytest --collect-only -q` for test discovery and generates
/// commands with JUnit XML output for structured result parsing.
///
/// # Configuration
///
/// See [`PytestFrameworkConfig`] for available options including:
/// - `paths`: Directories to search
/// - `command`: Full pytest invocation command
/// - `run_args`: Extra arguments for execution only
/// - `discovery_args`: Extra arguments for discovery only
pub struct PytestFramework {
    config: PytestFrameworkConfig,
    /// The program to invoke (first token of `command`).
    program: String,
    /// Additional arguments parsed from `command` (tokens after the program).
    prefix_args: Vec<String>,
}

impl PytestFramework {
    /// Creates a new pytest framework, validating the command at construction time.
    pub fn new(config: PytestFrameworkConfig) -> FrameworkResult<Self> {
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

    /// Builds the full argument list (after the program) for the discovery command.
    ///
    /// `discovery_args` tokens are inserted after `--collect-only -q` and before the
    /// group filters; search paths come last.
    fn discovery_cmd_args(
        &self,
        search_paths: &[String],
        filters: &str,
    ) -> FrameworkResult<Vec<String>> {
        let mut args: Vec<String> = self.prefix_args.clone();
        args.push("--collect-only".to_string());
        args.push("-q".to_string());

        // Append discovery_args for test discovery only (not execution)
        if let Some(discovery_args) = &self.config.discovery_args {
            let tokens = shell_words::split(discovery_args).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid discovery_args '{}': {}",
                    discovery_args, e
                ))
            })?;
            args.extend(tokens);
        }

        // Add filters if provided
        if !filters.is_empty() {
            let tokens = shell_words::split(filters).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid filter string '{}': {}",
                    filters, e
                ))
            })?;
            args.extend(tokens);
        }

        args.extend(search_paths.iter().cloned());
        Ok(args)
    }

    /// Parse `pytest --collect-only -q` output to extract test records.
    fn parse_collect_output(&self, output: &str, group: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();

        for line in output.lines() {
            let trimmed = line.trim();
            // Simple format: tests/test_foo.py::test_bar
            if trimmed.contains("::") && !trimmed.starts_with('<') && !trimmed.contains(' ') {
                tests.push(TestRecord::new(trimmed, group));
            }
        }

        tests
    }
}

#[async_trait]
impl TestFramework for PytestFramework {
    async fn discover(
        &self,
        paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Add paths to search (caller-provided paths take precedence over config)
        let search_paths: Vec<String> = if paths.is_empty() {
            self.config
                .paths
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        } else {
            paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        };

        // Build the pytest --collect-only command
        let cmd_args = self.discovery_cmd_args(&search_paths, filters)?;
        let mut cmd = tokio::process::Command::new(&self.program);
        for arg in &cmd_args {
            cmd.arg(arg);
        }

        super::env::apply_discovery_env(
            &mut cmd,
            &self.config.env,
            self.config.prepend_path.as_deref(),
        )?;

        // Build a display string for the command before running it
        let mut cmd_parts: Vec<&str> = Vec::new();
        cmd_parts.push(&self.program);
        for arg in &self.prefix_args {
            cmd_parts.push(arg);
        }
        cmd_parts.push("--collect-only");
        cmd_parts.push("-q");
        if let Some(discovery_args) = &self.config.discovery_args {
            cmd_parts.push(discovery_args);
        }
        let filter_display: String;
        if !filters.is_empty() {
            filter_display = filters.to_string();
            cmd_parts.push(&filter_display);
        }
        for path in &search_paths {
            cmd_parts.push(path);
        }
        let cmd_display = cmd_parts.join(" ");

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && !stdout.contains("::") {
            let detail = discovery_error_detail(&stderr, &stdout);
            return Err(FrameworkError::DiscoveryFailed(format!(
                "pytest --collect-only failed ({}):\n  command: {}\n  {}",
                output.status, cmd_display, detail
            )));
        }

        let tests = self.parse_collect_output(&stdout, group);

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
        fail_fast: bool,
    ) -> Command {
        let mut cmd = Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd = cmd.arg(arg);
        }

        cmd = cmd
            .arg("-v")
            .arg("--tb=short")
            .arg(format!("--junitxml={}", result_path));

        if fail_fast {
            cmd = cmd.arg("-x");
        }

        // Append run_args for test execution only (not discovery)
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

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(test.id());
        }

        super::env::attach_execution_env(
            &mut cmd,
            &self.config.env,
            self.config.prepend_path.as_deref(),
        );

        cmd
    }

    fn resolve_test_ids(
        &self,
        testsuites: &mut [TestsuiteXml],
        batch_test_ids: &[String],
    ) -> FrameworkResult<()> {
        for testsuite in testsuites.iter_mut() {
            for testcase in &mut testsuite.testcases {
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
                            "Failed to resolve JUnit testcase: {}",
                            msg
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::PytestFrameworkConfig;
    use crate::framework::TestInstance;

    #[test]
    fn test_command_prefix_with_command() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(fw.program, "uv");
        assert_eq!(fw.prefix_args, vec!["run", "pytest"]);
        Ok(())
    }

    #[test]
    fn test_command_prefix_default() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(fw.program, "python");
        assert_eq!(fw.prefix_args, vec!["-m", "pytest"]);
        Ok(())
    }

    #[test]
    fn test_new_rejects_invalid_command() {
        let config = PytestFrameworkConfig {
            command: "unclosed 'quote".to_string(),
            ..Default::default()
        };
        assert!(PytestFramework::new(config).is_err());
    }

    #[test]
    fn test_new_rejects_empty_command() {
        let config = PytestFrameworkConfig {
            command: "".to_string(),
            ..Default::default()
        };
        assert!(PytestFramework::new(config).is_err());
    }

    #[test]
    fn test_discovery_cmd_args_without_discovery_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string()];
        let args = fw.discovery_cmd_args(&paths, "-m 'not slow'")?;
        assert_eq!(
            args,
            vec![
                "run",
                "pytest",
                "--collect-only",
                "-q",
                "-m",
                "not slow",
                "tests"
            ]
        );
        Ok(())
    }

    #[test]
    fn test_discovery_cmd_args_with_discovery_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            discovery_args: Some("--no-cov".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string()];
        let args = fw.discovery_cmd_args(&paths, "-m 'not slow'")?;
        assert_eq!(
            args,
            vec![
                "run",
                "pytest",
                "--collect-only",
                "-q",
                "--no-cov",
                "-m",
                "not slow",
                "tests"
            ]
        );
        Ok(())
    }

    #[test]
    fn test_discovery_cmd_args_with_discovery_args_no_filters()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            discovery_args: Some("--no-cov -p no:cacheprovider".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string(), "examples".to_string()];
        let args = fw.discovery_cmd_args(&paths, "")?;
        assert_eq!(
            args,
            vec![
                "-m",
                "pytest",
                "--collect-only",
                "-q",
                "--no-cov",
                "-p",
                "no:cacheprovider",
                "tests",
                "examples"
            ]
        );
        Ok(())
    }

    #[test]
    fn test_discovery_cmd_args_rejects_invalid_quoting() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            discovery_args: Some("--no-cov 'unclosed".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string()];
        match fw.discovery_cmd_args(&paths, "") {
            Err(e) => assert!(matches!(e, FrameworkError::DiscoveryFailed(_))),
            Ok(_) => return Err("expected DiscoveryFailed for unbalanced quoting".into()),
        }
        Ok(())
    }

    #[test]
    fn test_execution_command_with_run_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            run_args: Some("--no-cov --timeout=30".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "test-group");
        let tests = vec![TestInstance::new(&record)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert_eq!(cmd.program, "uv");
        assert!(cmd.args.contains(&"--no-cov".to_string()));
        assert!(cmd.args.contains(&"--timeout=30".to_string()));
        assert!(cmd.args.contains(&"tests/test_a.py::test_one".to_string()));
        Ok(())
    }

    #[test]
    fn test_execution_command_excludes_discovery_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            discovery_args: Some("--no-cov".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert!(!cmd.args.contains(&"--no-cov".to_string()));
        assert!(!cmd.args.contains(&"--collect-only".to_string()));
        Ok(())
    }

    #[test]
    fn test_execution_command_fail_fast() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", true);
        assert!(cmd.args.contains(&"-x".to_string()));

        let cmd_no = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert!(!cmd_no.args.contains(&"-x".to_string()));

        Ok(())
    }

    #[test]
    fn test_execution_command_attaches_env_and_prepend_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            env: HashMap::from([("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]),
            prepend_path: Some(vec![".venv/bin".to_string()]),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);

        // `{root}` is left unresolved; providers resolve it at execution time.
        assert_eq!(
            cmd.env,
            vec![("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]
        );
        assert_eq!(cmd.prepend_path, vec![".venv/bin".to_string()]);
        Ok(())
    }

    #[test]
    fn test_execution_command_default_env_and_prepend_path_empty()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);

        assert!(cmd.env.is_empty());
        assert!(cmd.prepend_path.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_discover_applies_env_and_prepend_path() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let script = dir.path().join("fake-pytest");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s::test_echoed\\n' \"$OFFLOAD_FAKE_ROOT\"\n",
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

        let config = PytestFrameworkConfig {
            command: "fake-pytest".to_string(),
            env: HashMap::from([("OFFLOAD_FAKE_ROOT".to_string(), "{root}/marker".to_string())]),
            prepend_path: Some(vec![dir.path().to_string_lossy().into_owned()]),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;

        let tests = fw.discover(&[], "", "grp").await?;

        let cwd = std::env::current_dir()?;
        let expected = format!("{}/marker::test_echoed", cwd.display());
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].id, expected);
        Ok(())
    }
}
