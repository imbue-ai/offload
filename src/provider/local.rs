//! Local process provider implementation.
//!
//! This provider runs tests as child processes on the local machine.
//! It's the simplest provider and requires no external dependencies.
//!
//! # When to Use
//!
//! - **Development**: Fast iteration without container overhead
//! - **Simple CI**: When containerization isn't available or needed
//! - **Debugging**: Direct access to processes and filesystem
//!
//! # Characteristics
//!
//! | Feature | Support |
//! |---------|---------|
//! | Isolation | None (shared filesystem and network) |
//! | Resource limits | Not supported |
//! | File transfer | Local copy operations |
//! | Streaming output | Supported |
//! | Parallel execution | Yes, via multiple processes |
//!
//! # Example Configuration
//!
//! ```toml
//! [provider]
//! type = "local"
//! working_dir = "/path/to/project"
//! shell = "/bin/bash"
//!
//! [provider.env]
//! PYTHONPATH = "/path/to/project/src"
//! ```
//!
//! # Example Usage
//!
//! ```no_run
//! use offload::provider::local::LocalProvider;
//! use offload::provider::{SandboxProvider, Sandbox, Command, OutputLine};
//! use offload::config::{LocalProviderConfig, SandboxConfig};
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let provider = LocalProvider::new(LocalProviderConfig::default());
//!
//!     let config = SandboxConfig {
//!         id: "test-1".to_string(),
//!         working_dir: None,
//!         env: vec![],
//!         copy_dirs: vec![],
//!     };
//!
//!     let sandbox = provider.create_sandbox(&config).await?;
//!     let mut stream = sandbox.exec_stream(&Command::new("echo").arg("hello")).await?;
//!     while let Some(line) = stream.next().await {
//!         match line {
//!             OutputLine::Stdout(s) => println!("{}", s),
//!             OutputLine::Stderr(s) => eprintln!("{}", s),
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::{
    Command, OutputLine, OutputStream, ProviderError, ProviderResult, Sandbox, SandboxProvider,
};
use crate::config::{LocalProviderConfig, SandboxConfig};

/// Provider that runs tests as local child processes.
///
/// This is the simplest provider implementation. Each sandbox is just
/// a logical grouping with a shared configuration - commands are run
/// as child processes of the offload process itself.
///
/// # Thread Safety
///
/// The provider is thread-safe and can be shared across async tasks.
pub struct LocalProvider {
    config: LocalProviderConfig,
}

impl LocalProvider {
    /// Creates a new process provider with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration specifying working directory, environment
    ///   variables, and shell to use
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::local::LocalProvider;
    /// use offload::config::LocalProviderConfig;
    ///
    /// // With defaults
    /// let provider = LocalProvider::new(LocalProviderConfig::default());
    ///
    /// // With custom config
    /// let config = LocalProviderConfig {
    ///     working_dir: Some("/app".into()),
    ///     shell: "/bin/bash".to_string(),
    ///     ..Default::default()
    /// };
    /// let provider = LocalProvider::new(config);
    /// ```
    pub fn new(config: LocalProviderConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl SandboxProvider for LocalProvider {
    type Sandbox = LocalSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<LocalSandbox> {
        let working_dir = config
            .working_dir
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| self.config.working_dir.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        Ok(LocalSandbox {
            id: config.id.clone(),
            working_dir,
            env: config.env.clone(),
            shell: self.config.shell.clone(),
        })
    }
}

/// A sandbox that runs commands as local child processes.
///
/// Each command is executed via the configured shell (default: `/bin/sh`).
/// The sandbox provides a consistent working directory and environment
/// for all commands.
///
/// # File Transfer
///
/// Upload and download operations are implemented as local file copies
/// relative to the working directory. This is useful for tests that
/// produce output files.
///
/// # Termination
///
/// Since processes are transient, termination is a no-op. The sandbox
/// can be safely dropped without cleanup.
pub struct LocalSandbox {
    id: String,
    working_dir: PathBuf,
    env: Vec<(String, String)>,
    shell: String,
}

#[async_trait]
impl Sandbox for LocalSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        // Write test file if line range is specified
        if let Some((start, end)) = cmd.test_file_lines {
            let content = tokio::fs::read_to_string("offload.tests")
                .await
                .map_err(|e| {
                    ProviderError::ExecFailed(format!("Failed to read offload.tests: {}", e))
                })?;
            let lines: Vec<&str> = content.lines().collect();
            let selected = lines.get(start - 1..end).unwrap_or(&[]).join("\n");
            let path = self.working_dir.join("tmp/offload.tests");
            tokio::fs::create_dir_all(path.parent().unwrap()).await.ok();
            tokio::fs::write(&path, &selected).await.map_err(|e| {
                ProviderError::ExecFailed(format!("Failed to write test file: {}", e))
            })?;
        }

        let shell_cmd = cmd.to_shell_string();

        let mut process = tokio::process::Command::new(&self.shell);
        process.arg("-c").arg(&shell_cmd);
        process.current_dir(&self.working_dir);

        for (key, value) in &self.env {
            process.env(key, value);
        }
        for (key, value) in &cmd.env {
            process.env(key, value);
        }

        if let Some(dir) = &cmd.working_dir {
            process.current_dir(dir);
        }

        process.stdout(Stdio::piped());
        process.stderr(Stdio::piped());

        let mut child = process
            .spawn()
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::ExecFailed("stdout not captured".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProviderError::ExecFailed("stderr not captured".to_string()))?;

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        let stdout_stream = tokio_stream::wrappers::LinesStream::new(stdout_reader.lines()).map(
            |line: Result<String, std::io::Error>| OutputLine::Stdout(line.unwrap_or_default()),
        );

        let stderr_stream = tokio_stream::wrappers::LinesStream::new(stderr_reader.lines()).map(
            |line: Result<String, std::io::Error>| OutputLine::Stderr(line.unwrap_or_default()),
        );

        // Merge stdout and stderr streams
        let combined = stream::select(stdout_stream, stderr_stream);

        Ok(Box::pin(combined))
    }

    async fn upload(&self, local: &Path, remote: &Path) -> ProviderResult<()> {
        // For process sandbox, just copy the file
        let dest = self.working_dir.join(remote);

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ProviderError::UploadFailed(e.to_string()))?;
        }

        if local.is_dir() {
            copy_dir_all(local, &dest)
                .await
                .map_err(|e| ProviderError::UploadFailed(e.to_string()))?;
        } else {
            tokio::fs::copy(local, &dest)
                .await
                .map_err(|e| ProviderError::UploadFailed(e.to_string()))?;
        }

        Ok(())
    }

    async fn download(&self, paths: &[(&Path, &Path)]) -> ProviderResult<()> {
        for (remote, local) in paths {
            let src = self.working_dir.join(remote);

            if let Some(parent) = local.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;
            }

            if src.is_dir() {
                copy_dir_all(&src, local)
                    .await
                    .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;
            } else {
                tokio::fs::copy(&src, local)
                    .await
                    .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;
            }
        }

        Ok(())
    }

    async fn terminate(&self) -> ProviderResult<()> {
        // Process sandboxes don't need explicit cleanup
        Ok(())
    }
}

/// Recursively copy a directory.
async fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dst).await?;

    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let ty = entry.file_type().await?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ty.is_dir() {
            Box::pin(copy_dir_all(&src_path, &dst_path)).await?;
        } else {
            tokio::fs::copy(&src_path, &dst_path).await?;
        }
    }

    Ok(())
}
