//! Remote execution provider using Connectors.
//!
//! This provider delegates execution to a `Connector`, which handles
//! the actual communication with remote compute (EC2, GCP, Fly, etc.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
use crate::connector::{Connector, ShellConnector};

/// Configuration for the connector-based provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorProviderConfig {
    /// The connector command (e.g., "uv run connector.py")
    pub connector: String,

    /// Working directory for running the connector
    pub working_dir: Option<PathBuf>,

    /// Timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    3600
}

/// Provider that delegates to a Connector.
pub struct ConnectorProvider<C: Connector> {
    connector: Arc<C>,
    sandboxes: Arc<Mutex<HashMap<String, ConnectorSandboxInfo>>>,
}

#[allow(dead_code)]
struct ConnectorSandboxInfo {
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl ConnectorProvider<ShellConnector> {
    /// Create a new provider from config.
    pub fn from_config(config: &ConnectorProviderConfig) -> Self {
        let mut connector = ShellConnector::new(&config.connector)
            .with_timeout(config.timeout_secs);

        if let Some(dir) = &config.working_dir {
            connector = connector.with_working_dir(dir.clone());
        }

        Self {
            connector: Arc::new(connector),
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<C: Connector> ConnectorProvider<C> {
    /// Create a new provider with a custom connector.
    pub fn new(connector: C) -> Self {
        Self {
            connector: Arc::new(connector),
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a reference to the underlying connector.
    pub fn connector(&self) -> &C {
        &self.connector
    }
}

#[async_trait]
impl<C: Connector + 'static> SandboxProvider for ConnectorProvider<C> {
    type Sandbox = ConnectorSandbox<C>;
    type Config = ConnectorProviderConfig;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<Self::Sandbox> {
        info!("Creating connector sandbox: {}", config.id);

        let info = ConnectorSandboxInfo {
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        Ok(ConnectorSandbox {
            id: config.id.clone(),
            connector: self.connector.clone(),
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

/// A sandbox that uses a Connector for execution.
pub struct ConnectorSandbox<C: Connector> {
    id: String,
    connector: Arc<C>,
}

#[async_trait]
impl<C: Connector + 'static> Sandbox for ConnectorSandbox<C> {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();

        // Build command args
        let mut args = vec![cmd.program.clone()];
        args.extend(cmd.args.clone());

        debug!("Executing via connector: {:?}", args);

        let result = self.connector.execute(&args).await?;

        Ok(ExecResult {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            duration: start.elapsed(),
        })
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let result = self.exec(cmd).await?;
        let lines: Vec<_> = result
            .stdout
            .lines()
            .map(|l| OutputLine::Stdout(l.to_string()))
            .chain(result.stderr.lines().map(|l| OutputLine::Stderr(l.to_string())))
            .collect();
        Ok(Box::pin(futures::stream::iter(lines)))
    }

    async fn upload(&self, _local: &Path, _remote: &Path) -> ProviderResult<()> {
        warn!("upload() not supported by ConnectorSandbox - files should be included in connector image");
        Ok(())
    }

    async fn download(&self, _remote: &Path, _local: &Path) -> ProviderResult<()> {
        warn!("download() not supported by ConnectorSandbox");
        Ok(())
    }

    async fn status(&self) -> ProviderResult<SandboxStatus> {
        Ok(SandboxStatus::Running)
    }

    async fn terminate(&self) -> ProviderResult<()> {
        Ok(())
    }
}

// =============================================================================
// Legacy RemoteProvider for backwards compatibility
// =============================================================================

/// Configuration for the remote execution provider (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProviderConfig {
    /// Command to execute tests remotely.
    pub execute_command: String,

    /// Optional setup command.
    pub setup_command: Option<String>,

    /// Optional teardown command.
    pub teardown_command: Option<String>,

    /// Working directory.
    pub working_dir: Option<String>,

    /// Environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for RemoteProviderConfig {
    fn default() -> Self {
        Self {
            execute_command: String::new(),
            setup_command: None,
            teardown_command: None,
            working_dir: None,
            env: HashMap::new(),
            timeout_secs: 3600,
        }
    }
}

/// Legacy remote provider - wraps ShellConnector.
pub struct RemoteProvider {
    config: RemoteProviderConfig,
    sandboxes: Arc<Mutex<HashMap<String, RemoteSandboxInfo>>>,
}

#[allow(dead_code)]
struct RemoteSandboxInfo {
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl RemoteProvider {
    pub fn new(config: RemoteProviderConfig) -> Self {
        Self {
            config,
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl SandboxProvider for RemoteProvider {
    type Sandbox = RemoteSandbox;
    type Config = RemoteProviderConfig;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<Self::Sandbox> {
        info!("Creating remote sandbox: {}", config.id);

        let info = RemoteSandboxInfo {
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        Ok(RemoteSandbox {
            id: config.id.clone(),
            execute_command: self.config.execute_command.clone(),
            working_dir: self.config.working_dir.clone(),
            env: self.config.env.clone(),
            timeout_secs: self.config.timeout_secs,
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
        "remote"
    }
}

/// Legacy remote sandbox.
pub struct RemoteSandbox {
    id: String,
    execute_command: String,
    working_dir: Option<String>,
    env: HashMap<String, String>,
    timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
struct ExecuteOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

#[async_trait]
impl Sandbox for RemoteSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();

        let full_command = format!("{} {}", self.execute_command, cmd.to_shell_string());

        debug!("Executing remotely: {}", full_command);

        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(&full_command);

        if let Some(dir) = &self.working_dir {
            process.current_dir(dir);
        }

        for (key, value) in &self.env {
            process.env(key, value);
        }
        for (key, value) in &cmd.env {
            process.env(key, value);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            process.output(),
        )
        .await
        .map_err(|_| {
            ProviderError::Timeout(format!(
                "Remote execution timed out after {}s",
                self.timeout_secs
            ))
        })?
        .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        let duration = start.elapsed();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Try to parse JSON from last line
        let json_result = stdout
            .lines()
            .rev()
            .find(|line| line.trim().starts_with('{'))
            .and_then(|line| serde_json::from_str::<ExecuteOutput>(line).ok());

        match json_result {
            Some(parsed) => Ok(ExecResult {
                exit_code: parsed.exit_code,
                stdout: parsed.stdout,
                stderr: parsed.stderr,
                duration,
            }),
            None => Ok(ExecResult {
                exit_code: output.status.code().unwrap_or(-1),
                stdout,
                stderr,
                duration,
            }),
        }
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let result = self.exec(cmd).await?;
        let lines: Vec<_> = result
            .stdout
            .lines()
            .map(|l| OutputLine::Stdout(l.to_string()))
            .chain(result.stderr.lines().map(|l| OutputLine::Stderr(l.to_string())))
            .collect();
        Ok(Box::pin(futures::stream::iter(lines)))
    }

    async fn upload(&self, _local: &Path, _remote: &Path) -> ProviderResult<()> {
        warn!("upload() called on RemoteSandbox - file transfers should be handled by your execute script");
        Ok(())
    }

    async fn download(&self, _remote: &Path, _local: &Path) -> ProviderResult<()> {
        warn!("download() called on RemoteSandbox - file transfers should be handled by your execute script");
        Ok(())
    }

    async fn status(&self) -> ProviderResult<SandboxStatus> {
        Ok(SandboxStatus::Running)
    }

    async fn terminate(&self) -> ProviderResult<()> {
        Ok(())
    }
}
