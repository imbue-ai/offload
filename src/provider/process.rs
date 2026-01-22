//! Local process provider implementation.
//!
//! This provider runs tests as local processes, which is useful for
//! development, debugging, and environments where containerization
//! is not available.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use super::{
    Command, ExecResult, OutputLine, OutputStream, ProviderError, ProviderResult, Sandbox,
    SandboxInfo, SandboxProvider, SandboxStatus,
};
use crate::config::{ProcessProviderConfig, SandboxConfig};

/// Local process provider.
pub struct ProcessProvider {
    config: ProcessProviderConfig,
    sandboxes: Arc<Mutex<Vec<SandboxInfo>>>,
}

impl ProcessProvider {
    /// Create a new process provider with the given configuration.
    pub fn new(config: ProcessProviderConfig) -> Self {
        Self {
            config,
            sandboxes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl SandboxProvider for ProcessProvider {
    type Sandbox = ProcessSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<ProcessSandbox> {
        let working_dir = config
            .working_dir
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| self.config.working_dir.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let sandbox = ProcessSandbox {
            id: config.id.clone(),
            working_dir,
            env: config.env.clone(),
            shell: self.config.shell.clone(),
            status: Arc::new(Mutex::new(SandboxStatus::Running)),
        };

        // Track the sandbox
        let info = SandboxInfo {
            id: config.id.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.push(info);

        Ok(sandbox)
    }

    async fn list_sandboxes(&self) -> ProviderResult<Vec<SandboxInfo>> {
        Ok(self.sandboxes.lock().await.clone())
    }

    fn name(&self) -> &'static str {
        "process"
    }
}

/// A sandbox that runs commands as local processes.
pub struct ProcessSandbox {
    id: String,
    working_dir: PathBuf,
    env: Vec<(String, String)>,
    shell: String,
    status: Arc<Mutex<SandboxStatus>>,
}

#[async_trait]
impl Sandbox for ProcessSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();

        // Build the shell command
        let shell_cmd = cmd.to_shell_string();

        let mut process = tokio::process::Command::new(&self.shell);
        process.arg("-c").arg(&shell_cmd);
        process.current_dir(&self.working_dir);

        // Set environment variables
        for (key, value) in &self.env {
            process.env(key, value);
        }
        for (key, value) in &cmd.env {
            process.env(key, value);
        }

        // Set working directory if specified
        if let Some(dir) = &cmd.working_dir {
            process.current_dir(dir);
        }

        process.stdout(Stdio::piped());
        process.stderr(Stdio::piped());

        let output = if let Some(timeout) = cmd.timeout_secs {
            tokio::time::timeout(std::time::Duration::from_secs(timeout), process.output())
                .await
                .map_err(|_| {
                    ProviderError::Timeout(format!("Command timed out after {}s", timeout))
                })?
                .map_err(|e| ProviderError::ExecFailed(e.to_string()))?
        } else {
            process
                .output()
                .await
                .map_err(|e| ProviderError::ExecFailed(e.to_string()))?
        };

        let duration = start.elapsed();

        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            duration,
        })
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
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

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

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

    async fn download(&self, remote: &Path, local: &Path) -> ProviderResult<()> {
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

        Ok(())
    }

    async fn status(&self) -> ProviderResult<SandboxStatus> {
        Ok(*self.status.lock().await)
    }

    async fn terminate(&self) -> ProviderResult<()> {
        *self.status.lock().await = SandboxStatus::Stopped;
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
