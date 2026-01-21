//! Provider traits and implementations for sandbox execution environments.
//!
//! This module defines the core abstractions that allow shotgun to work with
//! any cloud provider, container runtime, or remote execution environment.

pub mod docker;
pub mod process;
pub mod remote;
pub mod ssh;

use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

use crate::config::SandboxConfig;

/// Result type for provider operations.
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Errors that can occur during provider operations.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Failed to create sandbox: {0}")]
    CreateFailed(String),

    #[error("Failed to execute command: {0}")]
    ExecFailed(String),

    #[error("Failed to upload file: {0}")]
    UploadFailed(String),

    #[error("Failed to download file: {0}")]
    DownloadFailed(String),

    #[error("Sandbox not found: {0}")]
    NotFound(String),

    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Sandbox exhausted: {0}")]
    SandboxExhausted(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Provider-specific error: {0}")]
    Other(#[from] anyhow::Error),
}

/// Information about a sandbox instance.
#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub id: String,
    pub status: SandboxStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Status of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Sandbox is being created.
    Creating,
    /// Sandbox is running and ready for commands.
    Running,
    /// Sandbox has stopped.
    Stopped,
    /// Sandbox has failed.
    Failed,
    /// Sandbox is being terminated.
    Terminating,
}

/// A command to execute in a sandbox.
#[derive(Debug, Clone)]
pub struct Command {
    /// The program to run.
    pub program: String,
    /// Arguments to pass to the program.
    pub args: Vec<String>,
    /// Working directory (optional).
    pub working_dir: Option<String>,
    /// Environment variables to set.
    pub env: Vec<(String, String)>,
    /// Timeout in seconds (optional).
    pub timeout_secs: Option<u64>,
}

impl Command {
    /// Create a new command.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            working_dir: None,
            env: Vec::new(),
            timeout_secs: None,
        }
    }

    /// Add an argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add multiple arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Add an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set the timeout.
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Convert to a shell command string.
    pub fn to_shell_string(&self) -> String {
        let mut parts = vec![shell_escape(&self.program)];
        for arg in &self.args {
            parts.push(shell_escape(arg));
        }
        parts.join(" ")
    }
}

/// Result of executing a command.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Exit code (0 typically means success).
    pub exit_code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Duration of execution.
    pub duration: std::time::Duration,
}

impl ExecResult {
    /// Check if the command succeeded (exit code 0).
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// A line of output from a streaming command.
#[derive(Debug, Clone)]
pub enum OutputLine {
    Stdout(String),
    Stderr(String),
}

/// A boxed stream of output lines.
pub type OutputStream = Pin<Box<dyn Stream<Item = OutputLine> + Send>>;

/// A sandbox is an isolated execution environment.
///
/// Sandboxes can execute commands, transfer files, and be terminated.
/// They abstract over different execution backends like Docker containers,
/// SSH connections, or local processes.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Get the unique identifier for this sandbox.
    fn id(&self) -> &str;

    /// Execute a command and wait for completion.
    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult>;

    /// Execute a command and stream output.
    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream>;

    /// Upload a file or directory to the sandbox.
    async fn upload(&self, local: &Path, remote: &Path) -> ProviderResult<()>;

    /// Download a file or directory from the sandbox.
    async fn download(&self, remote: &Path, local: &Path) -> ProviderResult<()>;

    /// Get the current status of the sandbox.
    async fn status(&self) -> ProviderResult<SandboxStatus>;

    /// Terminate the sandbox and clean up resources.
    async fn terminate(&self) -> ProviderResult<()>;
}

/// Escape a string for use in a shell command.
fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A type-erased sandbox for dynamic dispatch.
pub type DynSandbox = Box<dyn Sandbox>;

/// A provider creates and manages sandboxes.
///
/// Different providers implement different execution backends:
/// - Docker: Runs tests in containers
/// - SSH: Runs tests on remote machines
/// - Process: Runs tests as local processes
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// Create a new sandbox with the given configuration.
    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DynSandbox>;

    /// List all sandboxes managed by this provider.
    async fn list_sandboxes(&self) -> ProviderResult<Vec<SandboxInfo>>;

    /// Get the provider name (for logging and config).
    fn name(&self) -> &'static str;
}
