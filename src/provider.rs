//! Provider traits and implementations for sandbox execution environments.
//!
//! This module defines the core abstractions for executing tests in isolated
//! environments. The provider system is designed to be pluggable, allowing
//! offload to work with any execution backend: local processes, or
//! custom cloud providers.
//!
//! # Architecture
//!
//! The provider system has two main traits:
//!
//! - [`SandboxProvider`] - Factory that creates sandbox instances
//! - [`Sandbox`] - An isolated execution environment for running commands
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     SandboxProvider                          │
//! │  (creates sandboxes)                                        │
//! │                                                              │
//! │  create_sandbox() ──────────► Sandbox                       │
//! └────────────────────────────────┼────────────────────────────┘
//!                                  │
//!                                  ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        Sandbox                               │
//! │  (isolated execution environment)                           │
//! │                                                              │
//! │  exec_stream(Command) ───► OutputStream                     │
//! │  upload(local, remote)                                      │
//! │  download(remote, local)                                    │
//! │  terminate()                                                 │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Built-in Providers
//!
//! | Provider | Module | Description |
//! |----------|--------|-------------|
//! | Local | [`local`] | Run tests as local child processes |
//! | Default | [`default`] | Run tests via custom shell commands |
//!
//! # Implementing a Custom Provider
//!
//! To add support for a new execution environment:
//!
//! 1. Implement [`Sandbox`] for your execution context
//! 2. Implement [`SandboxProvider`] to create your sandbox type
//!
//! ```no_run
//! use async_trait::async_trait;
//! use offload::provider::*;
//! use offload::config::SandboxConfig;
//!
//! struct MyCloudSandbox { /* ... */ }
//!
//! #[async_trait]
//! impl Sandbox for MyCloudSandbox {
//!     fn id(&self) -> &str { todo!() }
//!     async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> { todo!() }
//!     async fn upload(&self, local: &std::path::Path, remote: &std::path::Path) -> ProviderResult<()> { todo!() }
//!     async fn download(&self, paths: &[(&std::path::Path, &std::path::Path)]) -> ProviderResult<()> { todo!() }
//!     async fn terminate(&self) -> ProviderResult<()> { todo!() }
//! }
//!
//! struct MyCloudProvider { /* ... */ }
//!
//! #[async_trait]
//! impl SandboxProvider for MyCloudProvider {
//!     type Sandbox = MyCloudSandbox;
//!     async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<Self::Sandbox> { todo!() }
//! }
//! ```
//!
//! # Error Handling
//!
//! All provider operations return [`ProviderResult<T>`], which wraps
//! [`ProviderError`]. Errors are categorized by failure type to enable
//! appropriate handling (e.g., retry on timeout, fail fast on auth errors).

pub mod default;
pub mod local;
pub mod modal;

use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

use crate::config::SandboxConfig;

/// Result type for provider operations.
///
/// All provider methods return this type, wrapping either a success value
/// or a [`ProviderError`] describing what went wrong.
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Errors that can occur during provider operations.
///
/// Errors are categorized to enable appropriate handling strategies:
/// - **Retryable**: `Timeout`, `Connection` - may succeed on retry
/// - **Fatal**: `CreateFailed`, `NotFound` - unlikely to succeed on retry
/// - **Resource**: `SandboxExhausted` - need to wait for resources
///
/// # Example
///
/// ```no_run
/// use offload::provider::{ProviderError, ProviderResult};
///
/// fn handle_error(result: ProviderResult<()>) {
///     match result {
///         Ok(()) => println!("Success"),
///         Err(ProviderError::Timeout(msg)) => println!("Timed out: {}", msg),
///         Err(ProviderError::Connection(msg)) => println!("Connection failed: {}", msg),
///         Err(e) => println!("Error: {}", e),
///     }
/// }
/// ```
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Failed to create a new sandbox instance.
    ///
    /// Common causes: image not found, insufficient resources, auth failure.
    #[error("Failed to create sandbox: {0}")]
    CreateFailed(String),

    /// Failed to execute a command in the sandbox.
    ///
    /// Note: A command that runs but returns non-zero exit code is NOT an error.
    /// This error indicates the command couldn't be started or communication failed.
    #[error("Failed to execute command: {0}")]
    ExecFailed(String),

    /// Failed to upload a file to the sandbox.
    #[error("Failed to upload file: {0}")]
    UploadFailed(String),

    /// Failed to download a file from the sandbox.
    #[error("Failed to download file: {0}")]
    DownloadFailed(String),

    /// The specified sandbox was not found.
    ///
    /// May indicate the sandbox was terminated or never existed.
    #[error("Sandbox not found: {0}")]
    NotFound(String),

    /// Failed to establish or maintain connection to the execution environment.
    #[error("Connection error: {0}")]
    Connection(String),

    /// Operation timed out.
    ///
    /// The command or operation took longer than the configured timeout.
    /// Consider increasing timeouts for long-running tests.
    #[error("Timeout: {0}")]
    Timeout(String),

    /// No more sandboxes can be created (resource limit reached).
    ///
    /// Wait for existing sandboxes to complete before creating more.
    #[error("Sandbox exhausted: {0}")]
    SandboxExhausted(String),

    /// I/O error during file operations.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Provider-specific error not covered by other variants.
    #[error("Provider-specific error: {0}")]
    Other(#[from] anyhow::Error),
}

/// A command to execute in a sandbox.
///
/// Commands are built using a fluent builder API and can be converted
/// to shell strings for execution.
///
/// # Example
///
/// ```
/// use offload::provider::Command;
///
/// let cmd = Command::new("pytest")
///     .arg("-v")
///     .arg("--tb=short")
///     .args(["tests/test_math.py::test_add", "tests/test_math.py::test_sub"])
///     .working_dir("/app")
///     .env("PYTHONPATH", "/app/src")
///     .timeout(300);
///
/// assert_eq!(cmd.program, "pytest");
/// assert_eq!(cmd.args.len(), 4);
/// ```
#[derive(Debug, Clone)]
pub struct Command {
    /// The program/executable to run.
    pub program: String,

    /// Arguments to pass to the program.
    pub args: Vec<String>,

    /// Working directory for command execution.
    ///
    /// If `None`, uses the sandbox's default working directory.
    pub working_dir: Option<String>,

    /// Environment variables to set for this command.
    ///
    /// These are merged with (and override) the sandbox's environment.
    pub env: Vec<(String, String)>,

    /// Maximum execution time in seconds.
    ///
    /// If the command runs longer, it will be terminated.
    pub timeout_secs: Option<u64>,

    /// Barrier count for synchronizing parallel exec calls.
    ///
    /// If > 0, the exec will wait until this many processes have fired
    /// before reading output. Used for profiling parallel execution.
    pub barrier_count: usize,
}

impl Command {
    /// Creates a new command with the given program.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let cmd = Command::new("python");
    /// ```
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            working_dir: None,
            env: Vec::new(),
            timeout_secs: None,
            barrier_count: 0,
        }
    }

    /// Adds a single argument to the command.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let cmd = Command::new("cargo").arg("test").arg("--release");
    /// ```
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Adds multiple arguments to the command.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let tests = vec!["test_a", "test_b", "test_c"];
    /// let cmd = Command::new("pytest").args(tests);
    /// ```
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets the working directory for command execution.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let cmd = Command::new("make").arg("test").working_dir("/project");
    /// ```
    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Adds an environment variable for this command.
    ///
    /// Can be called multiple times to add multiple variables.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let cmd = Command::new("pytest")
    ///     .env("PYTHONPATH", "/app")
    ///     .env("DEBUG", "1");
    /// ```
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sets the execution timeout in seconds.
    ///
    /// Commands exceeding this limit will be terminated.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let cmd = Command::new("pytest").timeout(300); // 5 minute timeout
    /// ```
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Sets the barrier count for synchronizing parallel exec calls.
    ///
    /// When > 0, exec will wait until this many processes have fired
    /// before reading output. Used for profiling parallel execution.
    pub fn barrier(mut self, count: usize) -> Self {
        self.barrier_count = count;
        self
    }

    /// Converts the command to a shell-executable string.
    ///
    /// The program and arguments are properly escaped for shell execution.
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::Command;
    /// let cmd = Command::new("echo").arg("hello world");
    /// assert_eq!(cmd.to_shell_string(), "echo 'hello world'");
    /// ```
    pub fn to_shell_string(&self) -> String {
        let mut parts = vec![shell_escape(&self.program)];
        for arg in &self.args {
            parts.push(shell_escape(arg));
        }
        parts.join(" ")
    }
}

/// Result of executing a command in a sandbox.
///
/// Contains the exit code, captured output, and execution duration.
///
/// # Example
///
/// ```
/// use offload::provider::ExecResult;
/// use std::time::Duration;
///
/// let result = ExecResult {
///     exit_code: 0,
///     stdout: "All tests passed".to_string(),
///     stderr: String::new(),
///     duration: Duration::from_secs(5),
/// };
///
/// if result.success() {
///     println!("Tests passed in {:?}", result.duration);
/// } else {
///     println!("Tests failed with code {}", result.exit_code);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Exit code of the command.
    ///
    /// By convention, 0 indicates success and non-zero indicates failure.
    /// The specific meaning of non-zero codes depends on the program.
    pub exit_code: i32,

    /// Captured standard output.
    pub stdout: String,

    /// Captured standard error.
    pub stderr: String,

    /// Wall-clock time the command took to execute.
    pub duration: std::time::Duration,
}

impl ExecResult {
    /// Returns `true` if the command succeeded (exit code 0).
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::ExecResult;
    /// use std::time::Duration;
    ///
    /// let result = ExecResult {
    ///     exit_code: 0,
    ///     stdout: "All tests passed".into(),
    ///     stderr: String::new(),
    ///     duration: Duration::from_secs(5),
    /// };
    /// assert!(result.success());
    /// ```
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// A single line of output from a streaming command.
///
/// Used with [`Sandbox::exec_stream`] to process output in real-time.
/// Each line is tagged with its source (stdout or stderr), or indicates
/// the final exit code of the command.
#[derive(Debug, Clone)]
pub enum OutputLine {
    /// A line from standard output.
    Stdout(String),
    /// A line from standard error.
    Stderr(String),
    /// The exit code of the command (yielded last, after all output).
    ExitCode(i32),
}

/// A stream of output lines from a command.
///
/// Returned by [`Sandbox::exec_stream`] for processing output in real-time.
/// The stream yields [`OutputLine`] items as they become available.
///
/// # Example
///
/// ```no_run
/// use futures::StreamExt;
/// use offload::provider::{Command, OutputLine, Sandbox};
///
/// async fn stream_output(sandbox: &impl Sandbox) {
///     let cmd = Command::new("pytest").arg("-v");
///     let mut stream = sandbox.exec_stream(&cmd).await.unwrap();
///
///     while let Some(line) = stream.next().await {
///         match line {
///             OutputLine::Stdout(s) => println!("[out] {}", s),
///             OutputLine::Stderr(s) => eprintln!("[err] {}", s),
///         }
///     }
/// }
/// ```
pub type OutputStream = Pin<Box<dyn Stream<Item = OutputLine> + Send>>;

/// An isolated execution environment for running commands.
///
/// A sandbox represents a single execution context where test commands can
/// be run. It provides methods for:
///
/// - **Command execution**: Run commands with [`exec`](Self::exec) or
///   [`exec_stream`](Self::exec_stream)
/// - **File transfer**: Copy files with [`upload`](Self::upload) and
///   [`download`](Self::download)
/// - **Lifecycle management**: Check [`status`](Self::status) and
///   [`terminate`](Self::terminate) when done
///
/// # Thread Safety
///
/// Sandboxes must be `Send` to allow passing between async tasks.
/// Most implementations are also safe to share (`Sync`), but this is
/// not required by the trait.
#[async_trait]
pub trait Sandbox: Send {
    /// Returns the unique identifier for this sandbox.
    ///
    /// The ID is assigned during creation and remains constant for the
    /// sandbox's lifetime. It's used for logging, tracking, and cleanup.
    fn id(&self) -> &str;

    /// Executes a command and streams output in real-time.
    ///
    /// Returns immediately with a stream that yields output lines as they're
    /// produced. Useful for long-running commands or real-time progress monitoring.
    ///
    /// # Arguments
    /// * `cmd` - The command to execute
    ///
    /// # Returns
    ///
    /// A stream of [`OutputLine`] items (stdout/stderr lines).
    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream>;

    /// Uploads a file or directory to the sandbox.
    ///
    /// Copies files from the local filesystem into the sandbox's filesystem.
    /// For directory uploads, the entire tree is copied recursively.
    ///
    /// # Arguments
    ///
    /// * `local` - Path on the local filesystem
    /// * `remote` - Destination path inside the sandbox
    async fn upload(&self, local: &Path, remote: &Path) -> ProviderResult<()>;

    /// Downloads files or directories from the sandbox.
    ///
    /// Copies files from the sandbox's filesystem to the local filesystem.
    /// For directory downloads, the entire tree is copied recursively.
    /// Multiple files can be downloaded in a single call for efficiency.
    ///
    /// # Arguments
    ///
    /// * `paths` - Slice of (remote, local) path pairs where remote is the
    ///   path inside the sandbox and local is the destination path
    async fn download(&self, paths: &[(&Path, &Path)]) -> ProviderResult<()>;

    /// Terminates the sandbox and releases resources.
    ///
    /// After calling this method, the sandbox should not be used.
    /// Resources (containers, connections, etc.) are cleaned up.
    ///
    /// This method is idempotent - calling it multiple times is safe.
    async fn terminate(&self) -> ProviderResult<()>;
}

/// Escape a string for use in a shell command.
fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Factory for creating and managing sandbox instances.
///
/// A `SandboxProvider` represents an execution backend (local, etc.)
/// and is responsible for creating [`Sandbox`] instances on demand. The
/// provider manages the pool of sandboxes and tracks their lifecycle.
///
/// # Thread Safety
///
/// Providers must be both `Send` and `Sync` to allow sharing across
/// async tasks via scoped spawns.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use offload::provider::{SandboxProvider, Sandbox};
/// use offload::provider::local::LocalProvider;
/// use offload::config::SandboxConfig;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let provider = LocalProvider::new(Default::default());
///
///     let config = SandboxConfig {
///         id: "test-sandbox-1".to_string(),
///         working_dir: Some("/app".to_string()),
///         env: vec![("DEBUG".to_string(), "1".to_string())],
///         copy_dirs: vec![],
///     };
///
///     let sandbox = provider.create_sandbox(&config).await?;
///     println!("Created sandbox: {}", sandbox.id());
///
///     Ok(())
/// }
/// ```
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// The concrete [`Sandbox`] type created by this provider.
    ///
    /// Each provider creates a specific sandbox implementation
    type Sandbox: Sandbox;

    /// Creates a new sandbox with the given configuration.
    ///
    /// This method provisions a new isolated execution environment.
    /// The sandbox is ready for use when this method returns successfully.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration specifying sandbox ID, working directory,
    ///   environment variables, and resource limits
    ///
    /// # Errors
    ///
    /// - `ProviderError::CreateFailed` - Failed to create sandbox
    /// - `ProviderError::SandboxExhausted` - Resource limit reached
    /// - `ProviderError::Connection` - Failed to connect to backend
    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<Self::Sandbox>;

    /// Returns base environment variables from the provider configuration.
    ///
    /// These environment variables are applied to all sandboxes created by
    /// this provider. They are typically set in the provider's config file
    /// and merged with sandbox-specific environment variables.
    ///
    /// # Default Implementation
    ///
    /// Returns an empty vector. Providers with environment configuration
    /// should override this method.
    fn base_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}
