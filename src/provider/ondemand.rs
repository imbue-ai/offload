//! On-demand compute provider abstraction.
//!
//! This module provides a generic interface for spinning up compute on demand
//! from any cloud provider, then connecting via SSH for test execution.
//!
//! The pattern:
//! 1. ComputeSpawner creates a machine and returns SSH connection info
//! 2. OnDemandProvider uses SSH for all command execution
//! 3. ComputeSpawner destroys the machine when done
//!
//! This allows plugging in any compute backend: EC2, GCP, Fly.io, etc.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::{
    Command, ExecResult, OutputStream, OutputLine, ProviderError, ProviderResult, Sandbox,
    SandboxInfo, SandboxProvider, SandboxStatus,
};
use crate::config::SandboxConfig;

/// SSH connection information returned by a compute spawner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConnectionInfo {
    /// Hostname or IP address.
    pub host: String,
    /// SSH port (usually 22).
    pub port: u16,
    /// Username to connect as.
    pub user: String,
    /// Path to private key file (if using key auth).
    pub key_path: Option<String>,
    /// Password (if using password auth - not recommended).
    pub password: Option<String>,
}

/// Trait for spawning on-demand compute resources.
///
/// Implement this trait to add support for a new cloud provider.
/// The spawner is responsible for:
/// 1. Creating a compute instance with SSH access
/// 2. Returning connection information
/// 3. Destroying the instance when done
#[async_trait]
pub trait ComputeSpawner: Send + Sync {
    /// Spawn a new compute instance.
    ///
    /// Returns the instance ID and SSH connection info.
    /// The instance should have SSH daemon running and accessible.
    async fn spawn(&self, id: &str) -> ProviderResult<(String, SshConnectionInfo)>;

    /// Destroy a compute instance.
    async fn destroy(&self, instance_id: &str) -> ProviderResult<()>;

    /// Get the spawner name (for logging).
    fn name(&self) -> &'static str;
}

/// Generic on-demand compute provider.
///
/// Uses a ComputeSpawner to create machines, then connects via SSH.
pub struct OnDemandProvider<S: ComputeSpawner> {
    spawner: S,
    working_dir: String,
    env: Vec<(String, String)>,
    health_check_timeout_secs: u64,
    sandboxes: Arc<Mutex<HashMap<String, OnDemandSandboxInfo>>>,
}

#[allow(dead_code)] // Fields kept for debugging
struct OnDemandSandboxInfo {
    instance_id: String,
    ssh_info: SshConnectionInfo,
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl<S: ComputeSpawner> OnDemandProvider<S> {
    pub fn new(spawner: S, working_dir: String) -> Self {
        Self {
            spawner,
            working_dir,
            env: Vec::new(),
            health_check_timeout_secs: 120,
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_health_check_timeout(mut self, secs: u64) -> Self {
        self.health_check_timeout_secs = secs;
        self
    }

    /// Wait for SSH to become available.
    async fn wait_for_ssh(&self, ssh_info: &SshConnectionInfo) -> ProviderResult<()> {
        let start = Instant::now();
        let timeout = std::time::Duration::from_secs(self.health_check_timeout_secs);

        loop {
            if start.elapsed() > timeout {
                return Err(ProviderError::Timeout(
                    "Timed out waiting for SSH to become available".to_string(),
                ));
            }

            let mut cmd = tokio::process::Command::new("ssh");
            cmd.args(build_ssh_args(ssh_info));
            cmd.arg(format!("{}@{}", ssh_info.user, ssh_info.host));
            cmd.arg("echo healthy");
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());

            match cmd.status().await {
                Ok(status) if status.success() => {
                    info!("SSH is available after {:?}", start.elapsed());
                    return Ok(());
                }
                _ => {
                    debug!("SSH not ready yet, retrying...");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
}

#[async_trait]
impl<S: ComputeSpawner + 'static> SandboxProvider for OnDemandProvider<S> {
    type Sandbox = OnDemandSandbox;
    type Config = ();

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<Self::Sandbox> {
        info!("Spawning compute for sandbox: {}", config.id);

        // Spawn compute instance
        let (instance_id, ssh_info) = self.spawner.spawn(&config.id).await?;

        info!(
            "Compute spawned: {} -> {}@{}:{}",
            instance_id, ssh_info.user, ssh_info.host, ssh_info.port
        );

        // Wait for SSH
        self.wait_for_ssh(&ssh_info).await?;

        // Track sandbox
        let info = OnDemandSandboxInfo {
            instance_id: instance_id.clone(),
            ssh_info: ssh_info.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        // Build environment
        let mut env = self.env.clone();
        env.extend(config.env.clone());

        let working_dir = config
            .working_dir
            .clone()
            .unwrap_or_else(|| self.working_dir.clone());

        Ok(OnDemandSandbox {
            id: config.id.clone(),
            instance_id,
            ssh_info,
            working_dir,
            env,
            spawner_name: self.spawner.name().to_string(),
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
        "ondemand"
    }
}

/// A sandbox backed by on-demand compute, accessed via SSH.
pub struct OnDemandSandbox {
    id: String,
    instance_id: String,
    ssh_info: SshConnectionInfo,
    working_dir: String,
    env: Vec<(String, String)>,
    spawner_name: String,
}

impl OnDemandSandbox {
    fn ssh_command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("ssh");
        for arg in build_ssh_args(&self.ssh_info) {
            cmd.arg(arg);
        }
        cmd.arg(format!("{}@{}", self.ssh_info.user, self.ssh_info.host));
        cmd
    }
}

#[async_trait]
impl Sandbox for OnDemandSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();

        // Build full command with env and working dir
        let mut full_cmd = String::new();

        for (key, value) in &self.env {
            full_cmd.push_str(&format!("export {}='{}'; ", key, value.replace('\'', "'\\''")));
        }
        for (key, value) in &cmd.env {
            full_cmd.push_str(&format!("export {}='{}'; ", key, value.replace('\'', "'\\''")));
        }

        let working_dir = cmd.working_dir.as_ref().unwrap_or(&self.working_dir);
        full_cmd.push_str(&format!("cd '{}'; ", working_dir.replace('\'', "'\\''")));
        full_cmd.push_str(&cmd.to_shell_string());

        let mut ssh_cmd = self.ssh_command();
        ssh_cmd.arg(&full_cmd);

        let output = if let Some(timeout) = cmd.timeout_secs {
            tokio::time::timeout(std::time::Duration::from_secs(timeout), ssh_cmd.output())
                .await
                .map_err(|_| ProviderError::Timeout(format!("Command timed out after {}s", timeout)))?
                .map_err(|e| ProviderError::ExecFailed(e.to_string()))?
        } else {
            ssh_cmd
                .output()
                .await
                .map_err(|e| ProviderError::ExecFailed(e.to_string()))?
        };

        Ok(ExecResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            duration: start.elapsed(),
        })
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let result = self.exec(cmd).await?;
        let stdout_lines: Vec<_> = result
            .stdout
            .lines()
            .map(|l| OutputLine::Stdout(l.to_string()))
            .collect();
        let stderr_lines: Vec<_> = result
            .stderr
            .lines()
            .map(|l| OutputLine::Stderr(l.to_string()))
            .collect();
        let all_lines: Vec<_> = stdout_lines.into_iter().chain(stderr_lines).collect();
        Ok(Box::pin(futures::stream::iter(all_lines)))
    }

    async fn upload(&self, local: &Path, remote: &Path) -> ProviderResult<()> {
        let ssh_args = build_ssh_args_string(&self.ssh_info);
        let remote_path = format!(
            "{}@{}:{}",
            self.ssh_info.user, self.ssh_info.host, remote.display()
        );

        let output = tokio::process::Command::new("rsync")
            .args(["-avz", "--no-D", "-e", &ssh_args])
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
        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;
        }

        let ssh_args = build_ssh_args_string(&self.ssh_info);
        let remote_path = format!(
            "{}@{}:{}",
            self.ssh_info.user, self.ssh_info.host, remote.display()
        );

        let output = tokio::process::Command::new("rsync")
            .args(["-avz", "--no-D", "-e", &ssh_args])
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

    async fn status(&self) -> ProviderResult<SandboxStatus> {
        match self.exec(&Command::new("true")).await {
            Ok(_) => Ok(SandboxStatus::Running),
            Err(_) => Ok(SandboxStatus::Failed),
        }
    }

    async fn terminate(&self) -> ProviderResult<()> {
        info!(
            "Terminating {} instance: {}",
            self.spawner_name, self.instance_id
        );
        // Note: actual termination happens through the spawner
        // This is called by the orchestrator but the spawner holds the destroy logic
        Ok(())
    }
}

// Helper functions for SSH args
fn build_ssh_args(info: &SshConnectionInfo) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        "ConnectTimeout=30".to_string(),
        "-p".to_string(),
        info.port.to_string(),
    ];

    if let Some(key_path) = &info.key_path {
        args.push("-i".to_string());
        args.push(key_path.clone());
    }

    args
}

fn build_ssh_args_string(info: &SshConnectionInfo) -> String {
    let mut parts = vec![
        "ssh".to_string(),
        "-o StrictHostKeyChecking=no".to_string(),
        "-o UserKnownHostsFile=/dev/null".to_string(),
        "-o LogLevel=ERROR".to_string(),
        format!("-p {}", info.port),
    ];

    if let Some(key_path) = &info.key_path {
        parts.push(format!("-i {}", key_path));
    }

    parts.join(" ")
}

// ============================================================================
// Built-in Spawner Implementations
// ============================================================================

/// Command-based spawner - runs user-provided commands to create/destroy compute.
///
/// This is the most flexible option - users provide shell commands that:
/// - Create compute and output JSON with connection info
/// - Destroy compute given an instance ID
pub struct CommandSpawner {
    /// Command to spawn compute. Should output JSON: {"instance_id": "...", "host": "...", "port": 22, "user": "..."}
    pub spawn_command: String,
    /// Command to destroy compute. {instance_id} will be replaced.
    pub destroy_command: String,
    /// Optional SSH key path to use for connections.
    pub key_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommandSpawnOutput {
    instance_id: String,
    host: String,
    #[serde(default = "default_ssh_port")]
    port: u16,
    #[serde(default = "default_ssh_user")]
    user: String,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_ssh_user() -> String {
    "root".to_string()
}

#[async_trait]
impl ComputeSpawner for CommandSpawner {
    async fn spawn(&self, id: &str) -> ProviderResult<(String, SshConnectionInfo)> {
        let command = self.spawn_command.replace("{id}", id);

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await
            .map_err(|e| ProviderError::CreateFailed(format!("Spawn command failed: {}", e)))?;

        if !output.status.success() {
            return Err(ProviderError::CreateFailed(format!(
                "Spawn command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: CommandSpawnOutput = serde_json::from_str(stdout.trim()).map_err(|e| {
            ProviderError::CreateFailed(format!(
                "Failed to parse spawn output as JSON: {}. Output: {}",
                e, stdout
            ))
        })?;

        let ssh_info = SshConnectionInfo {
            host: parsed.host,
            port: parsed.port,
            user: parsed.user,
            key_path: self.key_path.clone(),
            password: None,
        };

        Ok((parsed.instance_id, ssh_info))
    }

    async fn destroy(&self, instance_id: &str) -> ProviderResult<()> {
        let command = self.destroy_command.replace("{instance_id}", instance_id);

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await
            .map_err(|e| ProviderError::Other(anyhow::anyhow!("Destroy command failed: {}", e)))?;

        if !output.status.success() {
            warn!(
                "Destroy command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "command"
    }
}
