//! SSH provider implementation.
//!
//! This provider runs tests on remote machines via SSH, which is useful
//! for distributed testing across cloud VMs or bare metal servers.

use std::collections::HashMap;
use std::path::Path;
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
use crate::config::{SandboxConfig, SshProviderConfig};

/// SSH provider for remote test execution.
pub struct SshProvider {
    config: SshProviderConfig,
    host_index: Arc<Mutex<usize>>,
    sandboxes: Arc<Mutex<HashMap<String, SshSandboxInfo>>>,
}

struct SshSandboxInfo {
    #[allow(dead_code)]
    host: String,
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl SshProvider {
    /// Create a new SSH provider with the given configuration.
    pub fn new(config: SshProviderConfig) -> Self {
        Self {
            config,
            host_index: Arc::new(Mutex::new(0)),
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get the next host in round-robin fashion.
    async fn next_host(&self) -> String {
        let mut index = self.host_index.lock().await;
        let host = self.config.hosts[*index % self.config.hosts.len()].clone();
        *index += 1;
        host
    }
}

#[async_trait]
impl SandboxProvider for SshProvider {
    type Sandbox = SshSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<SshSandbox> {
        let host = self.next_host().await;

        let working_dir = config
            .working_dir
            .clone()
            .or_else(|| self.config.working_dir.clone());

        // Track the sandbox
        let info = SshSandboxInfo {
            host: host.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        // Build SSH options
        let mut ssh_opts = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            format!("ConnectTimeout=30"),
        ];

        if self.config.disable_host_key_check {
            ssh_opts.push("-o".to_string());
            ssh_opts.push("StrictHostKeyChecking=no".to_string());
            ssh_opts.push("-o".to_string());
            ssh_opts.push("UserKnownHostsFile=/dev/null".to_string());
        }

        if let Some(key_path) = &self.config.key_path {
            let key = shellexpand::tilde(&key_path.to_string_lossy()).into_owned();
            ssh_opts.push("-i".to_string());
            ssh_opts.push(key);
        }

        ssh_opts.push("-p".to_string());
        ssh_opts.push(self.config.port.to_string());

        Ok(SshSandbox {
            id: config.id.clone(),
            host,
            user: self.config.user.clone(),
            ssh_opts,
            working_dir,
            env: config.env.clone(),
        })
    }

    async fn list_sandboxes(&self) -> ProviderResult<Vec<SandboxInfo>> {
        let sandboxes = self.sandboxes.lock().await;
        Ok(sandboxes
            .iter()
            .map(|(id, info)| SandboxInfo {
                id: id.clone(),
                status: info.status,
                created_at: info.created_at,
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "ssh"
    }
}

/// A sandbox backed by an SSH connection.
///
/// This implementation uses the system `ssh` command for simplicity
/// and maximum compatibility.
pub struct SshSandbox {
    id: String,
    host: String,
    user: String,
    ssh_opts: Vec<String>,
    working_dir: Option<String>,
    env: Vec<(String, String)>,
}

impl SshSandbox {
    /// Build the SSH destination string.
    fn ssh_dest(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// Build a full SSH command.
    fn ssh_command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("ssh");
        for opt in &self.ssh_opts {
            cmd.arg(opt);
        }
        cmd.arg(self.ssh_dest());
        cmd
    }
}

#[async_trait]
impl Sandbox for SshSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();

        // Build the full command with environment and working directory
        let mut full_cmd = String::new();

        // Add environment variables
        for (key, value) in &self.env {
            full_cmd.push_str(&format!(
                "export {}='{}'; ",
                key,
                value.replace('\'', "'\\''")
            ));
        }
        for (key, value) in &cmd.env {
            full_cmd.push_str(&format!(
                "export {}='{}'; ",
                key,
                value.replace('\'', "'\\''")
            ));
        }

        // Change to working directory
        if let Some(dir) = cmd.working_dir.as_ref().or(self.working_dir.as_ref()) {
            full_cmd.push_str(&format!("cd '{}'; ", dir.replace('\'', "'\\''")));
        }

        // Add the actual command
        full_cmd.push_str(&cmd.to_shell_string());

        // Execute via SSH
        let mut ssh_cmd = self.ssh_command();
        ssh_cmd.arg(&full_cmd);

        let output = if let Some(timeout) = cmd.timeout_secs {
            tokio::time::timeout(std::time::Duration::from_secs(timeout), ssh_cmd.output())
                .await
                .map_err(|_| {
                    ProviderError::Timeout(format!("Command timed out after {}s", timeout))
                })?
                .map_err(|e| ProviderError::ExecFailed(e.to_string()))?
        } else {
            ssh_cmd
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
        // Build the full command with environment and working directory
        let mut full_cmd = String::new();

        // Add environment variables
        for (key, value) in &self.env {
            full_cmd.push_str(&format!(
                "export {}='{}'; ",
                key,
                value.replace('\'', "'\\''")
            ));
        }
        for (key, value) in &cmd.env {
            full_cmd.push_str(&format!(
                "export {}='{}'; ",
                key,
                value.replace('\'', "'\\''")
            ));
        }

        // Change to working directory
        if let Some(dir) = cmd.working_dir.as_ref().or(self.working_dir.as_ref()) {
            full_cmd.push_str(&format!("cd '{}'; ", dir.replace('\'', "'\\''")));
        }

        // Add the actual command
        full_cmd.push_str(&cmd.to_shell_string());

        // Execute via SSH with streaming
        let mut ssh_cmd = self.ssh_command();
        ssh_cmd.arg(&full_cmd);
        ssh_cmd.stdout(Stdio::piped());
        ssh_cmd.stderr(Stdio::piped());

        let mut child = ssh_cmd
            .spawn()
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

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

        // Merge stdout and stderr streams
        let combined = stream::select(stdout_stream, stderr_stream);

        Ok(Box::pin(combined))
    }

    async fn upload(&self, local: &Path, remote: &Path) -> ProviderResult<()> {
        let remote_path = format!("{}:{}", self.ssh_dest(), remote.display());

        let mut scp_args = vec!["-r".to_string()];

        // Add SSH options
        for opt in &self.ssh_opts {
            scp_args.push("-o".to_string());
            // Extract just the option part after -o
            if let Some(eq_pos) = opt.find('=') {
                scp_args.push(opt[..eq_pos].to_string());
            }
        }

        let output = tokio::process::Command::new("scp")
            .args(&scp_args)
            .arg(local)
            .arg(&remote_path)
            .output()
            .await
            .map_err(|e| ProviderError::UploadFailed(e.to_string()))?;

        if !output.status.success() {
            return Err(ProviderError::UploadFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(())
    }

    async fn download(&self, remote: &Path, local: &Path) -> ProviderResult<()> {
        let remote_path = format!("{}:{}", self.ssh_dest(), remote.display());

        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;
        }

        let mut scp_args = vec!["-r".to_string()];

        for opt in &self.ssh_opts {
            scp_args.push("-o".to_string());
            if let Some(eq_pos) = opt.find('=') {
                scp_args.push(opt[..eq_pos].to_string());
            }
        }

        let output = tokio::process::Command::new("scp")
            .args(&scp_args)
            .arg(&remote_path)
            .arg(local)
            .output()
            .await
            .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;

        if !output.status.success() {
            return Err(ProviderError::DownloadFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(())
    }

    fn status(&self) -> SandboxStatus {
        // SSH sandboxes are considered running until dropped
        SandboxStatus::Running
    }

    async fn terminate(&self) -> ProviderResult<()> {
        // SSH sandboxes don't need explicit termination
        // The connection will be dropped when the sandbox is dropped
        Ok(())
    }
}
