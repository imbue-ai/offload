//! Connector trait for remote test execution.
//!
//! Connectors handle the bridge between shotgun and remote compute providers.
//! They can be implemented in any language - the trait defines the protocol.
//!
//! # Protocol
//!
//! A connector is an executable that supports two modes:
//!
//! ## Discovery Mode
//! ```bash
//! connector --discover <paths...>
//! ```
//! Output: One test ID per line to stdout
//! ```text
//! tests/test_math.py::test_addition
//! tests/test_math.py::test_subtraction
//! ```
//!
//! ## Run Mode
//! ```bash
//! connector <command> [args...]
//! ```
//! Output: JSON to stdout
//! ```json
//! {"exit_code": 0, "stdout": "...", "stderr": "..."}
//! ```
//!
//! # Example Connector (Python)
//! ```python
//! #!/usr/bin/env python3
//! import sys
//! import json
//!
//! if sys.argv[1] == "--discover":
//!     # Run pytest --collect-only on remote, print test IDs
//!     for test in discover_tests(sys.argv[2:]):
//!         print(test)
//! else:
//!     # Run command on remote, return JSON result
//!     result = run_remote(sys.argv[1:])
//!     print(json.dumps(result))
//! ```

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::discovery::TestCase;
use crate::provider::{ProviderError, ProviderResult};

/// Result from a connector execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

use crate::provider::{OutputStream, OutputLine};
use futures::stream::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Trait for connectors that bridge shotgun to remote compute.
///
/// Connectors handle:
/// 1. Discovering tests on remote compute
/// 2. Executing test commands on remote compute
///
/// The default implementation shells out to an external command,
/// allowing connectors to be written in any language (Python, Go, etc.)
#[async_trait]
pub trait Connector: Send + Sync {
    /// Discover tests at the given paths.
    ///
    /// Returns a list of test IDs (e.g., "test_file.py::test_name").
    async fn discover(&self, paths: &[PathBuf]) -> ProviderResult<Vec<String>>;

    /// Execute a command remotely.
    ///
    /// The command is typically a test runner invocation like:
    /// `["pytest", "test_file.py::test_name", "-v"]`
    async fn execute(&self, command: &[String]) -> ProviderResult<ConnectorResult>;

    /// Execute a command and stream the output.
    ///
    /// Returns a stream of output lines as they are produced.
    async fn execute_stream(&self, command: &[String]) -> ProviderResult<OutputStream>;

    /// Get the connector name (for logging).
    fn name(&self) -> &str;
}

/// A connector that shells out to an external command.
///
/// This allows connectors to be written in any language.
/// The command must follow the connector protocol:
/// - `<cmd> --discover <paths>` → prints test IDs, one per line
/// - `<cmd> <command...>` → prints JSON result
pub struct ShellConnector {
    /// The base command to run (e.g., "uv run connector.py")
    command: String,
    /// Working directory for the command
    working_dir: Option<PathBuf>,
    /// Timeout in seconds
    timeout_secs: u64,
}

impl ShellConnector {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            working_dir: None,
            timeout_secs: 3600,
        }
    }

    pub fn with_working_dir(mut self, dir: PathBuf) -> Self {
        self.working_dir = Some(dir);
        self
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Build the base command parts.
    fn command_parts(&self) -> Vec<String> {
        // Split command string into parts, respecting quotes
        shell_words::split(&self.command).unwrap_or_else(|_| vec![self.command.clone()])
    }
}

#[async_trait]
impl Connector for ShellConnector {
    async fn discover(&self, paths: &[PathBuf]) -> ProviderResult<Vec<String>> {
        let mut parts = self.command_parts();
        parts.push("--discover".to_string());
        for path in paths {
            parts.push(path.to_string_lossy().to_string());
        }

        debug!("Running discovery: {:?}", parts);

        let mut cmd = tokio::process::Command::new(&parts[0]);
        cmd.args(&parts[1..]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| ProviderError::Timeout("Discovery timed out".to_string()))?
        .map_err(|e| ProviderError::ExecFailed(format!("Failed to run connector: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Discovery command failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let tests: Vec<String> = stdout
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(tests)
    }

    async fn execute(&self, command: &[String]) -> ProviderResult<ConnectorResult> {
        let mut parts = self.command_parts();
        parts.extend(command.iter().cloned());

        debug!("Running command: {:?}", parts);

        let mut cmd = tokio::process::Command::new(&parts[0]);
        cmd.args(&parts[1..]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| ProviderError::Timeout("Execution timed out".to_string()))?
        .map_err(|e| ProviderError::ExecFailed(format!("Failed to run connector: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Try to parse JSON result from stdout
        // Look for the last line that looks like JSON
        let json_result = stdout
            .lines()
            .rev()
            .find(|line| line.trim().starts_with('{'))
            .and_then(|line| serde_json::from_str::<ConnectorResult>(line).ok());

        match json_result {
            Some(result) => Ok(result),
            None => {
                // Fall back to using raw output
                Ok(ConnectorResult {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                })
            }
        }
    }

    async fn execute_stream(&self, command: &[String]) -> ProviderResult<OutputStream> {
        let mut parts = self.command_parts();
        parts.extend(command.iter().cloned());

        debug!("Streaming command: {:?}", parts);

        let mut cmd = tokio::process::Command::new(&parts[0]);
        cmd.args(&parts[1..]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()
            .map_err(|e| ProviderError::ExecFailed(format!("Failed to spawn connector: {}", e)))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| ProviderError::ExecFailed("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take()
            .ok_or_else(|| ProviderError::ExecFailed("Failed to capture stderr".to_string()))?;

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        let stdout_stream = tokio_stream::wrappers::LinesStream::new(stdout_reader.lines())
            .map(|line| OutputLine::Stdout(line.unwrap_or_default()));

        let stderr_stream = tokio_stream::wrappers::LinesStream::new(stderr_reader.lines())
            .map(|line| OutputLine::Stderr(line.unwrap_or_default()));

        // Merge stdout and stderr streams
        let combined = futures::stream::select(stdout_stream, stderr_stream);

        Ok(Box::pin(combined))
    }

    fn name(&self) -> &str {
        &self.command
    }
}

/// Convert discovered test IDs into TestCase structs.
pub fn test_ids_to_cases(ids: Vec<String>) -> Vec<TestCase> {
    ids.into_iter()
        .map(|id| TestCase {
            id: id.clone(),
            name: id.split("::").last().unwrap_or(&id).to_string(),
            file: id.split("::").next().map(PathBuf::from),
            line: None,
            markers: Vec::new(),
            skipped: false,
            flaky: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_test_id() {
        let cases = test_ids_to_cases(vec![
            "tests/test_math.py::test_addition".to_string(),
            "tests/test_math.py::TestClass::test_method".to_string(),
        ]);

        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].name, "test_addition");
        assert_eq!(cases[0].file, Some(PathBuf::from("tests/test_math.py")));
        assert_eq!(cases[1].name, "test_method");
    }
}
