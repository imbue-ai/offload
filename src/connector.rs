//! Connector trait for remote test execution.
//!
//! Connectors handle the bridge between shotgun and remote compute providers.
//! A connector simply runs shell commands - the caller decides what commands to run.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::discovery::TestCase;
use crate::provider::{OutputLine, OutputStream, ProviderError, ProviderResult};

use futures::stream::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Result from a command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Trait for connectors that run shell commands.
///
/// A connector is a simple interface for running commands - either locally
/// or on remote compute. The caller decides what commands to run.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Run a command and return the result.
    async fn run(&self, command: &str) -> ProviderResult<ExecResult>;

    /// Run a command and stream the output.
    async fn run_stream(&self, command: &str) -> ProviderResult<OutputStream>;

    /// Get the connector name (for logging).
    fn name(&self) -> &str;
}

/// A connector that shells out to run commands locally.
pub struct ShellConnector {
    /// Working directory for commands
    working_dir: Option<PathBuf>,
    /// Timeout in seconds
    timeout_secs: u64,
}

impl ShellConnector {
    pub fn new() -> Self {
        Self {
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
}

impl Default for ShellConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Connector for ShellConnector {
    async fn run(&self, command: &str) -> ProviderResult<ExecResult> {
        debug!("Running: {}", command);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
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
        .map_err(|_| ProviderError::Timeout("Command timed out".to_string()))?
        .map_err(|e| ProviderError::ExecFailed(format!("Failed to run command: {}", e)))?;

        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    async fn run_stream(&self, command: &str) -> ProviderResult<OutputStream> {
        debug!("Streaming: {}", command);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::ExecFailed(format!("Failed to spawn: {}", e)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::ExecFailed("Failed to capture stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProviderError::ExecFailed("Failed to capture stderr".to_string()))?;

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        let stdout_stream = tokio_stream::wrappers::LinesStream::new(stdout_reader.lines())
            .map(|line| OutputLine::Stdout(line.unwrap_or_default()));

        let stderr_stream = tokio_stream::wrappers::LinesStream::new(stderr_reader.lines())
            .map(|line| OutputLine::Stderr(line.unwrap_or_default()));

        let combined = futures::stream::select(stdout_stream, stderr_stream);

        Ok(Box::pin(combined))
    }

    fn name(&self) -> &str {
        "shell"
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
