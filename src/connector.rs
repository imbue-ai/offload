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
//! use offload::connector::{Connector, ShellConnector};
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
//! use offload::connector::{Connector, ShellConnector};
//! use offload::provider::OutputLine;
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
/// use offload::connector::ExecResult;
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
/// use offload::connector::{Connector, ShellConnector};
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
    /// use offload::connector::ShellConnector;
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
    /// use offload::connector::ShellConnector;
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
    /// use offload::connector::ShellConnector;
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
        let expanded_command = bundled::expand_command(command).map_err(|e| {
            ProviderError::ExecFailed(format!("Offload error when expanding command: {}", e))
        })?;

        debug!("Running: {}", expanded_command);

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

        // Take ownership of stdout/stderr handles
        let stdout_handle = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::ExecFailed("Failed to capture stdout".to_string()))?;
        let stderr_handle = child
            .stderr
            .take()
            .ok_or_else(|| ProviderError::ExecFailed("Failed to capture stderr".to_string()))?;

        // Read stdout and stderr concurrently
        let stdout_reader = BufReader::new(stdout_handle);
        let stderr_reader = BufReader::new(stderr_handle);

        let stdout_task = tokio::spawn(async move {
            let mut lines = stdout_reader.lines();
            let mut output = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                // Stream stdout in real-time
                debug!("{}", line);
                output.push(line);
            }
            output.join("\n")
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = stderr_reader.lines();
            let mut output = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                // Stream stderr in real-time
                debug!("{}", line);
                output.push(line);
            }
            output.join("\n")
        });

        // Wait for process and output collection with timeout
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), async {
                let status = child.wait().await?;
                let stdout = stdout_task.await.unwrap_or_default();
                let stderr = stderr_task.await.unwrap_or_default();
                Ok::<_, std::io::Error>((status, stdout, stderr))
            })
            .await
            .map_err(|_| ProviderError::Timeout("Command timed out".to_string()))?
            .map_err(|e| ProviderError::ExecFailed(format!("Failed to run command: {}", e)))?;

        let (status, stdout, stderr) = result;

        Ok(ExecResult {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
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

        // After stdout/stderr close, wait for child and yield exit code
        let exit_stream = futures::stream::once(async move {
            let exit_code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            OutputLine::ExitCode(exit_code)
        });

        Ok(Box::pin(combined.chain(exit_stream)))
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

    #[tokio::test]
    async fn test_run_stream_yields_exit_code_success() {
        let connector = ShellConnector::new();
        let mut stream = connector.run_stream("echo hello").await.unwrap();

        let mut exit_code = None;
        while let Some(line) = stream.next().await {
            if let OutputLine::ExitCode(code) = line {
                exit_code = Some(code);
            }
        }

        assert_eq!(exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_run_stream_yields_exit_code_failure() {
        let connector = ShellConnector::new();
        let mut stream = connector.run_stream("exit 42").await.unwrap();

        let mut exit_code = None;
        while let Some(line) = stream.next().await {
            if let OutputLine::ExitCode(code) = line {
                exit_code = Some(code);
            }
        }

        assert_eq!(exit_code, Some(42));
    }

    #[tokio::test]
    async fn test_run_stream_exit_code_comes_last() {
        let connector = ShellConnector::new();
        let mut stream = connector
            .run_stream("echo line1; echo line2")
            .await
            .unwrap();

        let mut lines = Vec::new();
        while let Some(line) = stream.next().await {
            lines.push(line);
        }

        // Exit code should be the last item
        assert!(lines.len() >= 3); // at least 2 stdout lines + exit code
        assert!(matches!(lines.last(), Some(OutputLine::ExitCode(0))));

        // All other items should be stdout/stderr
        for line in &lines[..lines.len() - 1] {
            assert!(matches!(
                line,
                OutputLine::Stdout(_) | OutputLine::Stderr(_)
            ));
        }
    }
}
