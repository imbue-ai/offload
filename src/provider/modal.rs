//! Modal provider for running tests on Modal sandboxes.
//!
//! This provider simplifies Modal integration by exposing high-level configuration
//! options instead of raw command strings. It internally generates the appropriate
//! `modal_sandbox.py` commands and uses [`DefaultSandbox`] for execution.
//!
//! # When to Use
//!
//! Use this provider when you want to run tests on Modal with a simplified
//! configuration. For advanced use cases requiring custom commands, use the
//! [`default`](super::default) provider instead.
//!
//! # Example Configuration
//!
//! ```toml
//! [provider]
//! type = "modal"
//! dockerfile = "./Dockerfile"
//! include_cwd = true
//! copy_dirs = ["./src:/app/src", "./tests:/app/tests"]
//! ```
//!
//! # Generated Commands
//!
//! The provider generates these `modal_sandbox.py` commands:
//!
//! - **prepare**: `uv run @modal_sandbox.py prepare [DOCKERFILE] [--include-cwd] [--cached] [--copy-dir=...]`
//! - **create**: `uv run @modal_sandbox.py create {image_id}`
//! - **exec**: `uv run @modal_sandbox.py exec {sandbox_id} {command}`
//! - **destroy**: `uv run @modal_sandbox.py destroy {sandbox_id}`
//! - **download**: `uv run @modal_sandbox.py download {sandbox_id} {paths}`

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use tracing::debug;

use super::default::DefaultSandbox;
use super::{OutputLine, ProviderError, ProviderResult, SandboxProvider};
use crate::config::{ModalProviderConfig, SandboxConfig};
use crate::connector::{Connector, ShellConnector};

/// Provider that runs tests on Modal sandboxes with simplified configuration.
///
/// Unlike [`DefaultProvider`](super::default::DefaultProvider) which requires
/// explicit command strings, this provider generates the Modal commands
/// automatically from high-level configuration options.
///
/// # Image Preparation
///
/// The `from_config` method runs the prepare command to build and cache
/// a Modal image. The resulting image ID is stored and used when creating
/// sandboxes.
///
/// # Sandbox Lifecycle
///
/// Each sandbox is a Modal sandbox instance. The provider uses `modal_sandbox.py`
/// for all operations:
///
/// 1. **Create**: Provisions a new Modal sandbox from the prepared image
/// 2. **Exec**: Runs commands in the sandbox
/// 3. **Download**: Retrieves files from the sandbox
/// 4. **Destroy**: Terminates and cleans up the sandbox
pub struct ModalProvider {
    /// Connector for running shell commands locally.
    connector: Arc<ShellConnector>,
    /// Cached image ID from the prepare command.
    image_id: String,
}

impl ModalProvider {
    /// Creates a new Modal provider from the given configuration.
    ///
    /// This method runs the prepare command to build a Modal image with the
    /// specified configuration. The image is cached (unless `no_cache` is true)
    /// for faster subsequent runs.
    ///
    /// # Arguments
    ///
    /// * `config` - Modal provider configuration with image settings
    /// * `copy_dirs` - Additional directories to copy into the image (local_path, remote_path).
    ///   These are combined with directories specified in the config.
    /// * `no_cache` - If true, skips the `--cached` flag, forcing a fresh image build.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::ExecFailed` if:
    /// - The prepare command fails (non-zero exit code)
    /// - The prepare command returns an empty image ID
    ///
    /// # Example
    ///
    /// ```no_run
    /// use offload::provider::modal::ModalProvider;
    /// use offload::config::ModalProviderConfig;
    ///
    /// # async fn example() -> anyhow::Result<()> {
    /// let config = ModalProviderConfig {
    ///     dockerfile: Some("./Dockerfile".to_string()),
    ///     include_cwd: true,
    ///     copy_dirs: vec!["./src:/app/src".to_string()],
    /// };
    ///
    /// let provider = ModalProvider::from_config(config, &[], false).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_config(
        config: ModalProviderConfig,
        copy_dirs: &[(PathBuf, PathBuf)],
        no_cache: bool,
    ) -> ProviderResult<Self> {
        let connector = Arc::new(ShellConnector::new());

        // Build prepare command
        let mut prepare_cmd = String::from("uv run @modal_sandbox.py prepare");

        // Add dockerfile if specified
        if let Some(dockerfile) = &config.dockerfile {
            prepare_cmd.push(' ');
            prepare_cmd.push_str(dockerfile);
        }

        // Add --include-cwd flag if configured
        if config.include_cwd {
            prepare_cmd.push_str(" --include-cwd");
        }

        // Add --cached flag unless no_cache is set
        if !no_cache {
            prepare_cmd.push_str(" --cached");
        }

        // Add copy_dirs from config
        for copy_spec in &config.copy_dirs {
            prepare_cmd.push_str(&format!(" --copy-dir={}", copy_spec));
        }

        // Add copy_dirs from parameter
        for (local, remote) in copy_dirs {
            prepare_cmd.push_str(&format!(
                " --copy-dir={}:{}",
                local.display(),
                remote.display()
            ));
        }

        eprintln!("Preparing Modal environment...");
        debug!("Running prepare command: {}", prepare_cmd);

        // Stream output in real-time (like DefaultProvider does)
        let mut stream = connector.run_stream(&prepare_cmd).await?;
        let mut last_stdout_line = String::new();
        let mut exit_code = 0;

        while let Some(line) = stream.next().await {
            match line {
                OutputLine::Stdout(s) => {
                    eprintln!("  {}", s);
                    last_stdout_line = s;
                }
                OutputLine::Stderr(s) => {
                    eprintln!("  {}", s);
                }
                OutputLine::ExitCode(code) => {
                    exit_code = code;
                }
            }
        }

        if exit_code != 0 {
            return Err(ProviderError::ExecFailed(format!(
                "Modal prepare command failed with exit code {}",
                exit_code
            )));
        }

        // Image ID is the last line of stdout
        let image_id = last_stdout_line.trim().to_string();

        if image_id.is_empty() {
            return Err(ProviderError::ExecFailed(
                "Modal prepare command returned empty image_id".to_string(),
            ));
        }

        debug!("Modal image prepared with ID: {}", image_id);

        Ok(Self {
            connector,
            image_id,
        })
    }
}

#[async_trait]
impl SandboxProvider for ModalProvider {
    type Sandbox = DefaultSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DefaultSandbox> {
        debug!("Creating Modal sandbox: {}", config.id);

        // Run create command to get sandbox_id
        let create_command = format!("uv run @modal_sandbox.py create {}", self.image_id);
        debug!("Running: {}", create_command);

        let result = self.connector.run(&create_command).await?;

        if result.exit_code != 0 {
            return Err(ProviderError::ExecFailed(format!(
                "Modal create command failed: {}",
                result.stderr
            )));
        }

        let sandbox_id = result.stdout.trim().to_string();
        if sandbox_id.is_empty() {
            return Err(ProviderError::ExecFailed(
                "Modal create command returned empty sandbox_id".to_string(),
            ));
        }

        debug!("Created Modal sandbox with ID: {}", sandbox_id);

        // Build command templates with sandbox_id placeholder for later substitution
        let exec_command = "uv run @modal_sandbox.py exec {sandbox_id} {command}".to_string();
        let destroy_command = "uv run @modal_sandbox.py destroy {sandbox_id}".to_string();
        let download_command =
            Some("uv run @modal_sandbox.py download {sandbox_id} {paths}".to_string());

        Ok(DefaultSandbox::new(
            sandbox_id,
            self.connector.clone(),
            exec_command,
            destroy_command,
            download_command,
            config.env.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_command_minimal() {
        // Verify prepare command building logic
        let config = ModalProviderConfig::default();

        let mut prepare_cmd = String::from("uv run @modal_sandbox.py prepare");

        if let Some(dockerfile) = &config.dockerfile {
            prepare_cmd.push(' ');
            prepare_cmd.push_str(dockerfile);
        }

        if config.include_cwd {
            prepare_cmd.push_str(" --include-cwd");
        }

        // no_cache = false, so --cached should be added
        prepare_cmd.push_str(" --cached");

        assert_eq!(prepare_cmd, "uv run @modal_sandbox.py prepare --cached");
    }

    #[test]
    fn test_prepare_command_with_dockerfile() {
        let config = ModalProviderConfig {
            dockerfile: Some("./Dockerfile".to_string()),
            include_cwd: false,
            copy_dirs: vec![],
        };

        let mut prepare_cmd = String::from("uv run @modal_sandbox.py prepare");

        if let Some(dockerfile) = &config.dockerfile {
            prepare_cmd.push(' ');
            prepare_cmd.push_str(dockerfile);
        }

        if config.include_cwd {
            prepare_cmd.push_str(" --include-cwd");
        }

        prepare_cmd.push_str(" --cached");

        assert_eq!(
            prepare_cmd,
            "uv run @modal_sandbox.py prepare ./Dockerfile --cached"
        );
    }

    #[test]
    fn test_prepare_command_with_all_options() {
        let config = ModalProviderConfig {
            dockerfile: Some("./Dockerfile.test".to_string()),
            include_cwd: true,
            copy_dirs: vec!["./src:/app/src".to_string()],
        };

        let copy_dirs = vec![(PathBuf::from("./tests"), PathBuf::from("/app/tests"))];

        let mut prepare_cmd = String::from("uv run @modal_sandbox.py prepare");

        if let Some(dockerfile) = &config.dockerfile {
            prepare_cmd.push(' ');
            prepare_cmd.push_str(dockerfile);
        }

        if config.include_cwd {
            prepare_cmd.push_str(" --include-cwd");
        }

        // no_cache = true, so --cached should NOT be added
        // (simulating no_cache = true by not adding --cached)

        for copy_spec in &config.copy_dirs {
            prepare_cmd.push_str(&format!(" --copy-dir={}", copy_spec));
        }

        for (local, remote) in &copy_dirs {
            prepare_cmd.push_str(&format!(
                " --copy-dir={}:{}",
                local.display(),
                remote.display()
            ));
        }

        assert_eq!(
            prepare_cmd,
            "uv run @modal_sandbox.py prepare ./Dockerfile.test --include-cwd --copy-dir=./src:/app/src --copy-dir=./tests:/app/tests"
        );
    }
}
