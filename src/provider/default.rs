//! Remote execution provider using lifecycle-based shell commands.
//!
//! This provider enables integration with any execution environment by
//! defining shell commands for sandbox lifecycle management. It's designed
//! for cloud providers like Modal, AWS Lambda, Fly.io, or custom systems.
//!
//! # When to Use
//!
//! - **Cloud functions**: Modal, AWS Lambda, Google Cloud Functions
//! - **Custom orchestration**: Kubernetes pods, Nomad jobs
//! - **Specialized hardware**: GPU instances, ARM machines
//! - **Serverless execution**: On-demand compute scaling
//!
//! # Characteristics
//!
//! | Feature | Support |
//! |---------|---------|
//! | Isolation | Depends on backend |
//! | Resource limits | Depends on backend |
//! | File transfer | Not supported |
//! | Streaming output | Supported |
//! | Parallel execution | Yes |
//!
//! # Command Protocol
//!
//! The provider uses three commands for lifecycle management:
//!
//! 1. **create_command**: Creates a sandbox, prints ID to stdout
//! 2. **exec_command**: Runs a command, uses `{sandbox_id}` and `{command}` placeholders
//! 3. **destroy_command**: Cleans up, uses `{sandbox_id}` placeholder
//!
//! The exec command can return either plain text or JSON:
//! ```json
//! {"exit_code": 0, "stdout": "output", "stderr": "errors"}
//! ```
//!
//! # Example: Modal Integration
//!
//! ```toml
//! [provider]
//! type = "default"
//! create_command = "python -c 'import uuid; print(uuid.uuid4())'"
//! exec_command = "modal run --sandbox-id {sandbox_id} -- {command}"
//! destroy_command = "modal sandbox delete {sandbox_id}"
//! ```
//!
//! # Example: Custom Kubernetes Executor
//!
//! ```toml
//! [provider]
//! type = "default"
//! working_dir = "/path/to/scripts"
//! create_command = "./k8s-create-pod.sh"
//! exec_command = "./k8s-exec.sh {sandbox_id} {command}"
//! destroy_command = "./k8s-delete-pod.sh {sandbox_id}"
//! timeout_secs = 3600
//! ```

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
use crate::config::{DefaultProviderConfig, SandboxConfig};
use crate::connector::{Connector, ShellConnector};

/// Provider that uses shell commands for sandbox lifecycle management.
///
/// This provider is highly flexible - it delegates all operations to
/// user-defined shell commands. The commands can call any external tool,
/// script, or API.
///
/// # Sandbox Creation
///
/// The `create_command` is run and must print a unique sandbox ID to stdout.
/// This ID is then used in subsequent exec and destroy commands.
pub struct DefaultProvider {
    connector: Arc<ShellConnector>,
    config: DefaultProviderConfig,
    sandboxes: Mutex<HashMap<String, DefaultSandboxInfo>>,
}

#[allow(dead_code)]
struct DefaultSandboxInfo {
    remote_id: String,
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl DefaultProvider {
    /// Creates a new provider from the given configuration.
    ///
    /// The configuration specifies the shell commands used for sandbox
    /// lifecycle management: create, exec, and destroy.
    ///
    /// # Arguments
    ///
    /// * `config` - Remote provider configuration with command templates
    ///
    /// # Example
    ///
    /// ```
    /// use offload::provider::default::DefaultProvider;
    /// use offload::config::DefaultProviderConfig;
    ///
    /// let config = DefaultProviderConfig {
    ///     create_command: "uuidgen".to_string(),
    ///     exec_command: "echo 'Running on {sandbox_id}'; {command}".to_string(),
    ///     destroy_command: "echo 'Cleaned up {sandbox_id}'".to_string(),
    ///     working_dir: None,
    ///     timeout_secs: 3600,
    ///     dockerfile_path: None,
    /// };
    ///
    /// let provider = DefaultProvider::from_config(config);
    /// ```
    pub fn from_config(config: DefaultProviderConfig) -> Self {
        let mut connector = ShellConnector::new().with_timeout(config.timeout_secs);

        if let Some(dir) = &config.working_dir {
            connector = connector.with_working_dir(dir.clone());
        }

        Self {
            connector: Arc::new(connector),
            config,
            sandboxes: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SandboxProvider for DefaultProvider {
    type Sandbox = DefaultSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DefaultSandbox> {
        info!("Creating connector sandbox: {}", config.id);

        // Expand placeholders in create command
        let create_cmd = self.config.create_command.replace(
            "{dockerfile_path}",
            self.config.dockerfile_path.as_deref().unwrap_or(""),
        );

        // Run the create command to get a sandbox_id
        let result = self.connector.run(&create_cmd).await?;

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

        let info = DefaultSandboxInfo {
            remote_id: remote_id.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        Ok(DefaultSandbox {
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
}

/// A sandbox managed through shell command templates.
///
/// The sandbox maintains a `remote_id` (returned by the create command)
/// that is substituted into the exec and destroy command templates.
///
/// # Reusability
///
/// Unlike single-use sandboxes, this sandbox can execute multiple commands
/// on the same remote instance. This is useful for stateful workflows where
/// subsequent commands depend on previous ones.
///
/// # File Transfer
///
/// File upload/download is not supported by this provider. If you need
/// file transfer, include the files in your execution environment (e.g.,
/// baked into a container image) or use a different provider.
///
/// # JSON Protocol
///
/// The exec command can optionally return JSON on stdout for structured
/// results. If the last line of output is valid JSON with `exit_code`,
/// `stdout`, and `stderr` fields, those are used as the result.
pub struct DefaultSandbox {
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

impl DefaultSandbox {
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
impl Sandbox for DefaultSandbox {
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
            && let Ok(parsed) = serde_json::from_str::<crate::connector::ExecResult>(json_line)
        {
            return Ok(ExecResult {
                exit_code: parsed.exit_code,
                stdout: parsed.stdout,
                stderr: parsed.stderr,
                duration: start.elapsed(),
            });
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
        warn!(
            "upload() not supported by DefaultSandbox - files should be included in connector image"
        );
        Ok(())
    }

    async fn download(&self, _remote: &Path, _local: &Path) -> ProviderResult<()> {
        warn!("download() not supported by DefaultSandbox");
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
