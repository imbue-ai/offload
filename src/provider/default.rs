//! Default execution provider using lifecycle-based shell commands.
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
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use super::{Command, OutputStream, ProviderError, ProviderResult, Sandbox, SandboxProvider};
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
///
/// # Image Preparation
///
/// If `prepare_command` is configured, it runs once during provider creation
/// via `from_config` and returns an image ID. This image ID is then substituted
/// into `create_command` via the `{image_id}` placeholder.
pub struct DefaultProvider {
    connector: Arc<ShellConnector>,
    config: DefaultProviderConfig,
    /// Cached image ID from prepare command (set during from_config).
    image_id: Option<String>,
}

impl DefaultProvider {
    /// Creates a new provider from the given configuration.
    ///
    /// The configuration specifies the shell commands used for sandbox
    /// lifecycle management: create, exec, and destroy.
    ///
    /// If `prepare_command` is configured, it runs during this method and
    /// the resulting image ID is stored for use in subsequent `create_sandbox` calls.
    ///
    /// # Arguments
    ///
    /// * `config` - Remote provider configuration with command templates
    /// * `copy_dirs` - Directories to copy into the image (local_path, remote_path).
    ///   These are baked into the image during prepare, making sandbox creation faster.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::ExecFailed` if the prepare command fails.
    pub async fn from_config(
        config: DefaultProviderConfig,
        copy_dirs: &[(std::path::PathBuf, std::path::PathBuf)],
    ) -> ProviderResult<Self> {
        let mut connector = ShellConnector::new().with_timeout(config.timeout_secs);

        if let Some(dir) = &config.working_dir {
            connector = connector.with_working_dir(dir.clone());
        }

        let connector = Arc::new(connector);

        // Run prepare command if configured
        let image_id = if let Some(prepare_cmd) = &config.prepare_command {
            info!("Running prepare command...");

            // Build prepare command with copy_dirs (both TOML-configured and CLI-provided)
            let mut full_prepare_cmd = prepare_cmd.clone();
            for copy_spec in &config.copy_dirs {
                info!("  Adding --copy-dir={} (from config)", copy_spec);
                full_prepare_cmd.push_str(&format!(" --copy-dir={}", copy_spec));
            }
            for (local, remote) in copy_dirs {
                info!(
                    "  Adding --copy-dir={}:{} (from CLI)",
                    local.display(),
                    remote.display()
                );
                full_prepare_cmd.push_str(&format!(
                    " --copy-dir={}:{}",
                    local.display(),
                    remote.display()
                ));
            }

            let result = connector.run(&full_prepare_cmd).await?;

            // Note: stderr is now streamed in real-time by the connector

            if result.exit_code != 0 {
                return Err(ProviderError::ExecFailed(format!(
                    "Prepare command failed: {}",
                    result.stderr
                )));
            }

            // Image id is the last line of stdout
            let image_id = result
                .stdout
                .lines()
                .last()
                .unwrap_or("")
                .trim()
                .to_string();

            if image_id.is_empty() {
                return Err(ProviderError::ExecFailed(
                    "Prepare command returned empty image_id".to_string(),
                ));
            }

            info!("Prepare command returned image_id: {}", image_id);
            Some(image_id)
        } else {
            None
        };

        Ok(Self {
            connector,
            config,
            image_id,
        })
    }
}

#[async_trait]
impl SandboxProvider for DefaultProvider {
    type Sandbox = DefaultSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DefaultSandbox> {
        info!("Creating default sandbox: {}", config.id);

        // Build the create command, substituting {image_id} if available
        // Note: copy_dirs are already baked into the image during prepare
        let create_command = match self.image_id.as_ref() {
            Some(id) => self.config.create_command.replace("{image_id}", id),
            None => self.config.create_command.clone(),
        };

        info!("{}", create_command);

        // Run the create command to get a sandbox_id
        // Note: stderr is streamed in real-time by the connector
        let result = self.connector.run(&create_command).await?;

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

        info!("Created default sandbox with ID: {}", remote_id);

        Ok(DefaultSandbox {
            id: config.id.clone(),
            remote_id,
            connector: self.connector.clone(),
            exec_command: self.config.exec_command.clone(),
            destroy_command: self.config.destroy_command.clone(),
            download_command: self.config.download_command.clone(),
        })
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
    /// Optional command template for downloading files
    download_command: Option<String>,
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

    /// Build the download command with substitutions.
    ///
    /// # Arguments
    ///
    /// * `paths` - Vector of (remote_path, local_path) tuples
    fn build_download_command(&self, paths: &[(String, String)]) -> Option<String> {
        self.download_command.as_ref().map(|cmd| {
            // Build paths string: "remote1:local1" "remote2:local2" ...
            let paths_str = paths
                .iter()
                .map(|(remote, local)| {
                    format!(
                        "{}:{}",
                        shell_words::quote(remote),
                        shell_words::quote(local)
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");

            cmd.replace("{sandbox_id}", &self.remote_id)
                .replace("{paths}", &paths_str)
        })
    }
}

#[async_trait]
impl Sandbox for DefaultSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let shell_cmd = self.build_exec_command(cmd);
        debug!("Streaming on {}: {}", self.remote_id, shell_cmd);
        self.connector.run_stream(&shell_cmd).await
    }

    async fn upload(&self, _local: &Path, _remote: &Path) -> ProviderResult<()> {
        warn!(
            "upload() not supported by DefaultSandbox - files should be included in execution environment"
        );
        Ok(())
    }

    async fn download(&self, paths: &[(&Path, &Path)]) -> ProviderResult<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let path_pairs: Vec<(String, String)> = paths
            .iter()
            .map(|(remote, local)| {
                (
                    remote.to_string_lossy().to_string(),
                    local.to_string_lossy().to_string(),
                )
            })
            .collect();

        if let Some(shell_cmd) = self.build_download_command(&path_pairs) {
            debug!(
                "Downloading from {}: {} path(s)",
                self.remote_id,
                paths.len()
            );
            let result = self.connector.run(&shell_cmd).await?;

            if result.exit_code != 0 {
                return Err(ProviderError::DownloadFailed(format!(
                    "Download command failed: {}",
                    result.stderr
                )));
            }

            for (remote, local) in &path_pairs {
                info!("Downloaded {} -> {}", remote, local);
            }
            Ok(())
        } else {
            Ok(())
        }
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
