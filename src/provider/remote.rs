//! Remote execution provider using Connectors.
//!
//! This provider delegates execution to a `Connector`, which handles
//! the actual communication with remote compute (EC2, GCP, Fly, etc.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::{
    Command, DynSandbox, ExecResult, OutputStream, ProviderError, ProviderResult, Sandbox,
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
    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DynSandbox> {
        info!("Creating connector sandbox: {}", config.id);

        let info = ConnectorSandboxInfo {
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        Ok(Box::new(ConnectorSandbox {
            id: config.id.clone(),
            connector: self.connector.clone(),
            can_run_command: AtomicBool::new(true),
        }))
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
///
/// This is a single-use sandbox: each exec() call spawns a new remote instance.
/// Calling exec() more than once will return `SandboxExhausted`.
pub struct ConnectorSandbox<C: Connector> {
    id: String,
    connector: Arc<C>,
    /// Whether this sandbox can still run a command. Set to false after first exec.
    /// For single-use sandboxes, this prevents wasteful duplicate remote invocations.
    can_run_command: AtomicBool,
}

#[async_trait]
impl<C: Connector + 'static> Sandbox for ConnectorSandbox<C> {
    fn id(&self) -> &str {
        &self.id
    }

    fn is_single_use(&self) -> bool {
        true
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        // Enforce single-use: only allow one exec call
        if !self.can_run_command.swap(false, Ordering::SeqCst) {
            return Err(ProviderError::SandboxExhausted(
                "ConnectorSandbox can only execute one command; each call spawns a new remote instance".to_string()
            ));
        }

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
        // Enforce single-use: only allow one exec call
        if !self.can_run_command.swap(false, Ordering::SeqCst) {
            return Err(ProviderError::SandboxExhausted(
                "ConnectorSandbox can only execute one command; each call spawns a new remote instance".to_string()
            ));
        }

        // Build command args
        let mut args = vec![cmd.program.clone()];
        args.extend(cmd.args.clone());

        debug!("Streaming via connector: {:?}", args);

        self.connector.execute_stream(&args).await
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
