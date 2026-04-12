//! Provider traits and implementations for sandbox execution environments.

pub mod default;
pub mod local;
pub mod modal;
pub mod retry;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::{Stream, StreamExt};

use crate::config::SandboxConfig;
use crate::connector::{Connector, ShellConnector};

/// Result type for provider operations.
///
/// All provider methods return this type, wrapping either a success value
/// or a [`ProviderError`] describing what went wrong.
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Context for building a sandbox image from a checkpoint rather than from scratch.
///
/// When present, providers append `--from-checkpoint`, `--checkpoint-sha`, and
/// `--sandbox-project-root` flags to the prepare command and omit the normal
/// `--include-cwd`, `--copy-dir`, and `--sandbox-init-cmd` flags.
#[derive(Debug, Clone)]
pub struct CheckpointContext {
    /// Modal image ID of the checkpoint image to build on top of.
    pub image_id: String,
    /// Git commit SHA that the checkpoint image was built from.
    pub commit_sha: String,
    /// Project root path inside the sandbox (e.g. `/app`).
    pub sandbox_project_root: String,
}

/// Estimated compute cost of a sandbox or aggregated run.
#[derive(Clone, Debug, Default)]
pub struct CostEstimate {
    /// Total CPU-seconds consumed.
    pub cpu_seconds: f64,
    /// Estimated cost in USD.
    pub estimated_cost_usd: f64,
}

impl std::fmt::Display for CostEstimate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Estimated cost: ${:.4} ({:.1} CPU-seconds)",
            self.estimated_cost_usd, self.cpu_seconds
        )
    }
}

/// Errors that can occur during provider operations.
///
/// Errors are categorized to enable appropriate handling strategies:
/// - **Retryable**: `Timeout`, `Connection` - may succeed on retry
/// - **Fatal**: `CreateFailed`, `NotFound` - unlikely to succeed on retry
/// - **Resource**: `SandboxExhausted` - need to wait for resources
///
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
}

impl Command {
    /// Creates a new command with the given program.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            working_dir: None,
            env: Vec::new(),
            timeout_secs: None,
        }
    }

    /// Adds a single argument to the command.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Adds multiple arguments to the command.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets the working directory for command execution.
    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Sets the execution timeout in seconds.
    ///
    /// Commands exceeding this limit will be terminated.
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// Converts the command to a shell-executable string.
    ///
    /// The program and arguments are properly escaped for shell execution.
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
pub type OutputStream = Pin<Box<dyn Stream<Item = OutputLine> + Send>>;

/// An isolated execution environment for running commands.
///
/// A sandbox represents a single execution context where test commands can
/// be run. It provides methods for: **Command execution**, **File download, **Lifecycle management
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

    /// Returns the estimated cost incurred by this sandbox.
    ///
    /// The cost is calculated based on elapsed time since sandbox creation
    /// and provider-specific pricing. For local sandboxes, returns zero cost.
    fn cost_estimate(&self) -> CostEstimate;
}

/// Streams a prepare command, buffering output while discovery is in-flight.
/// Returns the image_id (last stdout line) on success.
pub(crate) async fn run_prepare_command(
    connector: &ShellConnector,
    command: &str,
    label: &str,
    discovery_done: Option<&AtomicBool>,
) -> ProviderResult<String> {
    let mut buffer: Vec<String> = Vec::new();
    let emit = |msg: String, buf: &mut Vec<String>| {
        if discovery_done.is_some_and(|flag| !flag.load(Ordering::Acquire)) {
            buf.push(msg);
        } else {
            for buffered in buf.drain(..) {
                eprintln!("{}", buffered);
            }
            eprintln!("{}", msg);
        }
    };

    emit(
        format!("[prepare] Preparing {} environment...", label),
        &mut buffer,
    );

    let mut stream = connector.run_stream(command).await?;
    let mut last_stdout_line = String::new();
    let mut exit_code = 0;

    while let Some(line) = stream.next().await {
        match line {
            OutputLine::Stdout(s) => {
                emit(format!("[prepare]   {}", s), &mut buffer);
                last_stdout_line = s;
            }
            OutputLine::Stderr(s) => {
                emit(format!("[prepare]   {}", s), &mut buffer);
            }
            OutputLine::ExitCode(code) => {
                exit_code = code;
            }
        }
    }

    // Flush any remaining buffered output
    for buffered in buffer.drain(..) {
        eprintln!("{}", buffered);
    }

    if exit_code != 0 {
        return Err(ProviderError::ExecFailed(format!(
            "{} prepare command failed with exit code {}",
            label, exit_code
        )));
    }

    let image_id = last_stdout_line.trim().to_string();

    if image_id.is_empty() {
        return Err(ProviderError::ExecFailed(format!(
            "{} prepare command returned empty image_id",
            label
        )));
    }

    Ok(image_id)
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
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// The concrete [`Sandbox`] type created by this provider.
    ///
    /// Each provider creates a specific sandbox implementation
    type Sandbox: Sandbox;

    /// Runs provider preparation (e.g. image build) and returns an image ID.
    ///
    /// For providers that build images (Modal, Default with `prepare_command`),
    /// this runs the prepare command and caches the resulting image ID.
    /// For providers that do not build images (Local, Default without
    /// `prepare_command`), this is a no-op returning `None`.
    async fn prepare(
        &mut self,
        copy_dirs: &[(PathBuf, PathBuf)],
        no_cache: bool,
        sandbox_init_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
        context_dir: Option<&std::path::Path>,
    ) -> ProviderResult<Option<String>>;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_estimate_display_formats_cost_and_seconds() {
        let cost = CostEstimate {
            cpu_seconds: 123.4,
            estimated_cost_usd: 0.0048,
        };
        let display = format!("{}", cost);
        assert_eq!(display, "Estimated cost: $0.0048 (123.4 CPU-seconds)");
    }

    #[test]
    fn cost_estimate_display_zero() {
        let cost = CostEstimate::default();
        let display = format!("{}", cost);
        assert_eq!(display, "Estimated cost: $0.0000 (0.0 CPU-seconds)");
    }

    #[test]
    fn cost_estimate_default_is_zero() {
        let cost = CostEstimate::default();
        assert_eq!(cost.cpu_seconds, 0.0);
        assert_eq!(cost.estimated_cost_usd, 0.0);
    }

    #[test]
    fn cost_estimate_display_small_values() {
        let cost = CostEstimate {
            cpu_seconds: 0.5,
            estimated_cost_usd: 0.00001971,
        };
        let display = format!("{}", cost);
        assert!(
            display.contains("$0.0000"),
            "small cost rounds to 4 decimals: {display}"
        );
        assert!(
            display.contains("0.5 CPU-seconds"),
            "fractional seconds: {display}"
        );
    }

    #[test]
    fn cost_estimate_display_large_values() {
        let cost = CostEstimate {
            cpu_seconds: 50000.0,
            estimated_cost_usd: 1.971,
        };
        let display = format!("{}", cost);
        assert_eq!(display, "Estimated cost: $1.9710 (50000.0 CPU-seconds)");
    }

    #[test]
    fn cost_estimate_aggregation() {
        let costs = [
            CostEstimate {
                cpu_seconds: 10.0,
                estimated_cost_usd: 0.001,
            },
            CostEstimate {
                cpu_seconds: 20.0,
                estimated_cost_usd: 0.002,
            },
            CostEstimate {
                cpu_seconds: 30.0,
                estimated_cost_usd: 0.003,
            },
        ];

        let total = costs.iter().fold(CostEstimate::default(), |mut acc, cost| {
            acc.cpu_seconds += cost.cpu_seconds;
            acc.estimated_cost_usd += cost.estimated_cost_usd;
            acc
        });

        assert_eq!(total.cpu_seconds, 60.0);
        assert!((total.estimated_cost_usd - 0.006).abs() < f64::EPSILON);
    }

    #[test]
    fn cost_estimate_aggregation_empty() {
        let costs: Vec<CostEstimate> = vec![];
        let total = costs.iter().fold(CostEstimate::default(), |mut acc, cost| {
            acc.cpu_seconds += cost.cpu_seconds;
            acc.estimated_cost_usd += cost.estimated_cost_usd;
            acc
        });

        assert_eq!(total.cpu_seconds, 0.0);
        assert_eq!(total.estimated_cost_usd, 0.0);
    }
}
