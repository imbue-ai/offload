//! Connector trait for shell command execution.
//!
//! This module provides the [`Connector`] trait, a simple abstraction for running
//! shell commands either locally or on remote compute resources. Unlike the
//! [`Sandbox`](crate::provider::Sandbox) trait which provides a higher-level
//! interface for test execution, connectors are a lower-level primitive focused
//! purely on command execution.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │                   Caller                        │
//! │         (decides what commands to run)          │
//! └─────────────────────┬───────────────────────────┘
//!                       │
//!                       ▼
//! ┌─────────────────────────────────────────────────┐
//! │              Connector Trait                    │
//! │   run() - buffered execution                    │
//! │   run_stream() - streaming output               │
//! └─────────────────────┬───────────────────────────┘
//!                       │
//!           ┌───────────┴───────────┐
//!           ▼                       ▼
//! ┌─────────────────┐     ┌─────────────────┐
//! │ ShellConnector  │     │ Custom Connector│
//! │ (local shell)   │     │ (API, etc)      │
//! └─────────────────┘     └─────────────────┘
//! ```
//!
//! # Built-in Connectors
//!
//! | Connector | Description |
//! |-----------|-------------|
//! | [`ShellConnector`] | Executes commands via local shell (`sh -c`) |
//!
//! # Example: Using ShellConnector
//!
//! ```no_run
//! use shotgun::connector::{Connector, ShellConnector};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let connector = ShellConnector::new()
//!     .with_working_dir("/path/to/project".into())
//!     .with_timeout(300);
//!
//! let result = connector.run("pytest tests/ --collect-only -q").await?;
//!
//! if result.exit_code == 0 {
//!     println!("Tests discovered:\n{}", result.stdout);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Example: Streaming Output
//!
//! ```no_run
//! use shotgun::connector::{Connector, ShellConnector};
//! use shotgun::provider::OutputLine;
//! use futures::StreamExt;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let connector = ShellConnector::new();
//! let mut stream = connector.run_stream("pytest tests/ -v").await?;
//!
//! while let Some(line) = stream.next().await {
//!     match line {
//!         OutputLine::Stdout(s) => println!("{}", s),
//!         OutputLine::Stderr(s) => eprintln!("{}", s),
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::bundled;
use crate::provider::{OutputLine, OutputStream, ProviderError, ProviderResult};

use futures::stream::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Result from a shell command execution.
///
/// Contains the exit code and captured output from a command run via a
/// [`Connector`]. This is similar to [`provider::ExecResult`](crate::provider::ExecResult)
/// but without the duration field.
///
/// # Example
///
/// ```
/// use shotgun::connector::ExecResult;
///
/// let result = ExecResult {
///     exit_code: 0,
///     stdout: "test_add PASSED\ntest_sub PASSED\n".to_string(),
///     stderr: String::new(),
/// };
///
/// if result.exit_code == 0 {
///     println!("Command succeeded with output:\n{}", result.stdout);
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    /// Exit code from the command (0 typically indicates success).
    pub exit_code: i32,

    /// Captured standard output.
    pub stdout: String,

    /// Captured standard error.
    pub stderr: String,
}

/// Trait for connectors that execute shell commands.
///
/// A connector provides a minimal interface for running commands, either locally
/// or on remote compute resources. Unlike [`Sandbox`](crate::provider::Sandbox),
/// connectors don't manage lifecycle or provide file transfer - they simply
/// execute commands and return results.
///
/// # Thread Safety
///
/// Connectors must be `Send + Sync` as they may be shared across async tasks.
///
/// # Implementation Notes
///
/// When implementing a connector:
/// - Commands are passed as shell strings (e.g., `"pytest tests/ -v"`)
/// - Use `sh -c` or equivalent to execute commands
/// - Handle timeouts appropriately in your implementation
/// - Stream implementations should interleave stdout/stderr as they arrive
///
#[async_trait]
pub trait Connector: Send + Sync {
    /// Executes a command and returns the buffered result.
    ///
    /// The command is executed as a shell command (like `sh -c "command"`).
    /// Output is captured and returned after the command completes.
    ///
    /// # Arguments
    ///
    /// * `command` - Shell command string to execute
    ///
    /// # Returns
    ///
    /// The execution result including exit code and captured output.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Timeout`] if the command exceeds the
    /// configured timeout, or [`ProviderError::ExecFailed`] if the command
    /// cannot be started.
    async fn run(&self, command: &str) -> ProviderResult<ExecResult>;

    /// Executes a command and streams output as it occurs.
    ///
    /// Unlike [`run`](Self::run), this returns immediately with a stream
    /// that yields output lines as they become available. Useful for
    /// long-running commands or real-time progress display.
    ///
    /// # Arguments
    ///
    /// * `command` - Shell command string to execute
    ///
    /// # Returns
    ///
    /// A stream of [`OutputLine`] values,
    /// interleaving stdout and stderr as they arrive.
    ///
    /// # Note
    ///
    /// The stream does not provide the exit code. If you need the exit code,
    /// use [`run`](Self::run) instead or check command output for success/failure
    /// indicators.
    async fn run_stream(&self, command: &str) -> ProviderResult<OutputStream>;
}

/// A connector that executes commands via the local shell.
///
/// Uses `sh -c` to execute commands, providing a simple way to run
/// shell commands locally. Supports configurable working directory
/// and timeout.
///
/// # Default Configuration
///
/// | Setting | Default |
/// |---------|---------|
/// | Working directory | Current directory |
/// | Timeout | 3600 seconds (1 hour) |
///
/// # Example
///
/// ```no_run
/// use shotgun::connector::{Connector, ShellConnector};
///
/// # async fn example() -> anyhow::Result<()> {
/// // Basic usage
/// let connector = ShellConnector::new();
/// let result = connector.run("echo 'Hello, World!'").await?;
/// assert_eq!(result.exit_code, 0);
///
/// // With configuration
/// let connector = ShellConnector::new()
///     .with_working_dir("/path/to/project".into())
///     .with_timeout(300); // 5 minute timeout
///
/// let result = connector.run("make test").await?;
/// # Ok(())
/// # }
/// ```
///
/// # Platform Support
///
/// Requires a POSIX-compatible `sh` shell. Works on Linux, macOS, and
/// Windows with WSL or Git Bash.
pub struct ShellConnector {
    /// Working directory for commands
    working_dir: Option<PathBuf>,
    /// Timeout in seconds
    timeout_secs: u64,
}

impl ShellConnector {
    /// Creates a new shell connector with default settings.
    ///
    /// Uses the current working directory and a 1-hour timeout.
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::connector::ShellConnector;
    ///
    /// let connector = ShellConnector::new();
    /// ```
    pub fn new() -> Self {
        Self {
            working_dir: None,
            timeout_secs: 3600,
        }
    }

    /// Sets the working directory for command execution.
    ///
    /// Commands will be executed with this directory as their
    /// current working directory.
    ///
    /// # Arguments
    ///
    /// * `dir` - Path to the working directory
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::connector::ShellConnector;
    ///
    /// let connector = ShellConnector::new()
    ///     .with_working_dir("/home/user/project".into());
    /// ```
    pub fn with_working_dir(mut self, dir: PathBuf) -> Self {
        self.working_dir = Some(dir);
        self
    }

    /// Sets the command timeout in seconds.
    ///
    /// Commands that exceed this duration will be terminated and
    /// return a [`ProviderError::Timeout`].
    ///
    /// # Arguments
    ///
    /// * `secs` - Timeout duration in seconds
    ///
    /// # Example
    ///
    /// ```
    /// use shotgun::connector::ShellConnector;
    ///
    /// let connector = ShellConnector::new()
    ///     .with_timeout(600); // 10 minute timeout
    /// ```
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
        // Expand @filename.ext references to full paths
        let expanded_command = bundled::expand_command(command)
            .map_err(|e| ProviderError::ExecFailed(format!("Failed to expand command: {}", e)))?;

        debug!("Running: {}", expanded_command);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &expanded_command]);
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
        // Expand @filename.ext references to full paths
        let expanded_command = bundled::expand_command(command)
            .map_err(|e| ProviderError::ExecFailed(format!("Failed to expand command: {}", e)))?;

        debug!("Streaming: {}", expanded_command);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &expanded_command]);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::TestRecord;

    fn test_ids_to_records(ids: Vec<String>) -> Vec<TestRecord> {
        ids.into_iter()
            .map(|id| {
                let file = id.split("::").next().map(PathBuf::from);
                let mut record = TestRecord::new(id);
                if let Some(f) = file {
                    record = record.with_file(f);
                }
                record
            })
            .collect()
    }

    #[test]
    fn test_parse_test_id() {
        let records = test_ids_to_records(vec![
            "tests/test_math.py::test_addition".to_string(),
            "tests/test_math.py::TestClass::test_method".to_string(),
        ]);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "test_addition");
        assert_eq!(records[0].file, Some(PathBuf::from("tests/test_math.py")));
        assert_eq!(records[1].name, "test_method");
    }
}
