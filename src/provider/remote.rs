//! Remote execution provider using Connectors.
//!
//! This provider delegates execution to a `Connector`, which handles
//! running shell commands. The Sandbox manages the lifecycle (create/exec/destroy).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::{
    Command, ExecResult, OutputStream, ProviderError, ProviderResult, Sandbox, SandboxInfo,
    SandboxProvider, SandboxStatus,
};
use crate::config::{RemoteProviderConfig, SandboxConfig};
use crate::connector::{Connector, ShellConnector};

/// Provider that uses shell commands for lifecycle management.
pub struct ConnectorProvider {
    connector: Arc<ShellConnector>,
    config: RemoteProviderConfig,
    sandboxes: Arc<Mutex<HashMap<String, ConnectorSandboxInfo>>>,
}

#[allow(dead_code)]
struct ConnectorSandboxInfo {
    remote_id: String,
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl ConnectorProvider {
    /// Create a new provider from config.
    pub fn from_config(config: RemoteProviderConfig) -> Self {
        let mut connector = ShellConnector::new().with_timeout(config.timeout_secs);

        if let Some(dir) = &config.working_dir {
            connector = connector.with_working_dir(dir.clone());
        }

        Self {
            connector: Arc::new(connector),
            config,
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl SandboxProvider for ConnectorProvider {
    type Sandbox = ConnectorSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<ConnectorSandbox> {
        info!("Creating connector sandbox: {}", config.id);

        // Run the create command to get a sandbox_id
        let result = self.connector.run(&self.config.create_command).await?;

        if result.exit_code != 0 {
            return Err(ProviderError::ExecFailed(format!(
                "Create command failed: {}",
                result.stderr
            )));
        }

        let remote_id = result.stdout.trim().to_string();
        if remote_id.is_empty() {
            return Err(ProviderError::ExecFailed(
                "Create command returned empty sandbox_id".to_string(),
            ));
        }

        info!("Created remote sandbox: {}", remote_id);

        let info = ConnectorSandboxInfo {
            remote_id: remote_id.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        Ok(ConnectorSandbox {
            id: config.id.clone(),
            remote_id,
            connector: self.connector.clone(),
            exec_command: self.config.exec_command.clone(),
            destroy_command: self.config.destroy_command.clone(),
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
        "connector"
    }
}

/// A sandbox that uses shell commands for execution.
///
/// This sandbox is reusable: you can call exec() multiple times on the same
/// remote instance, then call terminate() to clean up.
pub struct ConnectorSandbox {
    /// Local sandbox ID
    id: String,
    /// Remote sandbox ID from create command
    remote_id: String,
    /// The connector for running commands
    connector: Arc<ShellConnector>,
    /// Command template for execution
    exec_command: String,
    /// Command template for destruction
    destroy_command: String,
}

impl ConnectorSandbox {
    /// Build the exec command with substitutions.
    fn build_exec_command(&self, cmd: &Command) -> String {
        // Build the inner command with properly escaped arguments
        let inner_cmd = std::iter::once(cmd.program.as_str())
            .chain(cmd.args.iter().map(|s| s.as_str()))
            .map(|a| shell_words::quote(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        // Escape the entire command so it can be passed as a single shell argument
        let escaped_cmd = shell_words::quote(&inner_cmd);

        self.exec_command
            .replace("{sandbox_id}", &self.remote_id)
            .replace("{command}", &escaped_cmd)
    }

    /// Build the destroy command with substitutions.
    fn build_destroy_command(&self) -> String {
        self.destroy_command
            .replace("{sandbox_id}", &self.remote_id)
    }
}

#[async_trait]
impl Sandbox for ConnectorSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();
        let shell_cmd = self.build_exec_command(cmd);

        debug!("Executing on {}: {}", self.remote_id, shell_cmd);

        let result = self.connector.run(&shell_cmd).await?;

        // Try to parse JSON result from stdout (connector protocol)
        if let Some(json_line) = result
            .stdout
            .lines()
            .rev()
            .find(|line| line.trim().starts_with('{'))
        {
            if let Ok(parsed) = serde_json::from_str::<crate::connector::ExecResult>(json_line) {
                return Ok(ExecResult {
                    exit_code: parsed.exit_code,
                    stdout: parsed.stdout,
                    stderr: parsed.stderr,
                    duration: start.elapsed(),
                });
            }
        }

        // Fall back to raw output
        Ok(ExecResult {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            duration: start.elapsed(),
        })
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let shell_cmd = self.build_exec_command(cmd);
        debug!("Streaming on {}: {}", self.remote_id, shell_cmd);
        self.connector.run_stream(&shell_cmd).await
    }

    async fn upload(&self, _local: &Path, _remote: &Path) -> ProviderResult<()> {
        warn!("upload() not supported by ConnectorSandbox - files should be included in connector image");
        Ok(())
    }

    async fn download(&self, _remote: &Path, _local: &Path) -> ProviderResult<()> {
        warn!("download() not supported by ConnectorSandbox");
        Ok(())
    }

    fn status(&self) -> SandboxStatus {
        SandboxStatus::Running
    }

    async fn terminate(&self) -> ProviderResult<()> {
        let shell_cmd = self.build_destroy_command();
        info!(
            "Terminating sandbox {} (remote: {})",
            self.id, self.remote_id
        );

        let result = self.connector.run(&shell_cmd).await?;

        if result.exit_code != 0 {
            warn!("Destroy command failed: {}", result.stderr);
        }

        Ok(())
    }
}
