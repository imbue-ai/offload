//! Modal provider — simplified configuration for running tests on Modal sandboxes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use async_trait::async_trait;
use tracing::debug;

use super::default::DefaultSandbox;
use super::{ProviderError, ProviderResult, SandboxProvider, run_prepare_command};
use crate::config::{ModalProviderConfig, SandboxConfig};
use crate::connector::{Connector, ShellConnector};

/// Provider that runs tests on Modal sandboxes with simplified configuration.
///
/// Unlike [`DefaultProvider`](super::default::DefaultProvider) which requires
/// explicit command strings, this provider generates the Modal commands
/// automatically from high-level configuration options.
///
/// # Lifecycle
///
/// 1. `from_config()` — lightweight construction, stores config only
/// 2. `prepare()` — runs the image build, stores the resulting image ID
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
    connector: Arc<ShellConnector>,
    config: ModalProviderConfig,
    /// Set during `prepare()`.
    image_id: Option<String>,
    env: Vec<(String, String)>,
    cpu_cores: f64,
}

impl ModalProvider {
    /// Creates a new Modal provider from the given configuration.
    ///
    /// This is a lightweight constructor that stores the config without
    /// performing any I/O. Call [`prepare()`](SandboxProvider::prepare) to
    /// run the image build.
    pub fn from_config(config: ModalProviderConfig) -> Self {
        let connector = Arc::new(ShellConnector::new());

        let env: Vec<(String, String)> = config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let cpu_cores = config.cpu_cores;

        Self {
            connector,
            config,
            image_id: None,
            env,
            cpu_cores,
        }
    }
}

#[async_trait]
impl SandboxProvider for ModalProvider {
    type Sandbox = DefaultSandbox;

    async fn prepare(
        &mut self,
        copy_dirs: &[(PathBuf, PathBuf)],
        no_cache: bool,
        sandbox_init_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
    ) -> ProviderResult<Option<String>> {
        let mut prepare_cmd = String::from("uv run @modal_sandbox.py prepare");

        if let Some(dockerfile) = &self.config.dockerfile {
            prepare_cmd.push(' ');
            prepare_cmd.push_str(dockerfile);
        }

        if self.config.include_cwd {
            prepare_cmd.push_str(" --include-cwd");
        }

        if !no_cache {
            prepare_cmd.push_str(" --cached");
        }

        for copy_spec in &self.config.copy_dirs {
            prepare_cmd.push_str(&format!(" --copy-dir={}", copy_spec));
        }

        for (local, remote) in copy_dirs {
            prepare_cmd.push_str(&format!(
                " --copy-dir={}:{}",
                local.display(),
                remote.display()
            ));
        }

        if let Some(init_cmd) = sandbox_init_cmd {
            prepare_cmd.push_str(&format!(
                " --sandbox-init-cmd={}",
                shell_words::quote(init_cmd)
            ));
        }

        debug!("Running prepare command: {}", prepare_cmd);

        let image_id =
            run_prepare_command(&self.connector, &prepare_cmd, "Modal", discovery_done).await?;

        debug!("Modal image prepared with ID: {}", image_id);

        self.image_id = Some(image_id.clone());
        Ok(Some(image_id))
    }

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DefaultSandbox> {
        debug!("Creating Modal sandbox: {}", config.id);

        // Run create command to get sandbox_id
        let image_id = self.image_id.as_deref().ok_or_else(|| {
            ProviderError::ExecFailed(
                "Modal image ID not set; call prepare() before create_sandbox()".to_string(),
            )
        })?;
        let create_command = format!(
            "uv run @modal_sandbox.py create --cpu {} {}",
            self.cpu_cores, image_id
        );
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

        // Merge provider base env with sandbox-specific env (includes OFFLOAD_ROOT)
        let mut env = self.base_env();
        env.extend(config.env.iter().cloned());

        Ok(DefaultSandbox::new(
            sandbox_id,
            self.connector.clone(),
            exec_command,
            destroy_command,
            download_command,
            env,
            Instant::now(),
            self.cpu_cores,
        ))
    }

    fn base_env(&self) -> Vec<(String, String)> {
        self.env.clone()
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
            env: Default::default(),
            cpu_cores: 0.125,
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
            env: Default::default(),
            cpu_cores: 0.125,
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
