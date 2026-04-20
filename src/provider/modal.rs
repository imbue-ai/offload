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
    memory_gb: Option<f64>,
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
        let memory_gb = config.memory_gb;

        Self {
            connector,
            config,
            image_id: None,
            env,
            cpu_cores,
            memory_gb,
        }
    }

    /// Builds the shell command string for the `prepare` step.
    fn build_prepare_command(
        &self,
        copy_dirs: &[(PathBuf, PathBuf)],
        sandbox_init_cmd: Option<&str>,
    ) -> String {
        let mut cmd = String::from("uv run @modal_sandbox.py prepare");

        if let Some(dockerfile) = &self.config.dockerfile {
            cmd.push(' ');
            cmd.push_str(&shell_words::quote(dockerfile));
        }

        if self.config.include_cwd {
            cmd.push_str(" --include-cwd");
        }

        for copy_spec in &self.config.copy_dirs {
            cmd.push_str(&format!(" --copy-dir={}", shell_words::quote(copy_spec)));
        }

        for (local, remote) in copy_dirs {
            let spec = format!("{}:{}", local.display(), remote.display());
            cmd.push_str(&format!(" --copy-dir={}", shell_words::quote(&spec)));
        }

        if let Some(init_cmd) = sandbox_init_cmd {
            cmd.push_str(&format!(
                " --sandbox-init-cmd={}",
                shell_words::quote(init_cmd)
            ));
        }

        cmd
    }

    /// Builds the shell command string for the `create` step.
    fn build_create_command(&self, image_id: &str) -> ProviderResult<String> {
        let mut cmd = format!(
            "uv run @modal_sandbox.py create --cpu {} {}",
            self.cpu_cores,
            shell_words::quote(image_id)
        );

        if let Some(gb) = self.memory_gb {
            cmd.push_str(&format!(" --memory-gb {}", gb));
        }

        if !self.config.experimental_options.is_empty() {
            let json = serde_json::to_string(&self.config.experimental_options).map_err(|e| {
                ProviderError::ExecFailed(format!("Failed to serialize experimental_options: {e}"))
            })?;
            cmd.push_str(&format!(
                " --experimental-options {}",
                shell_words::quote(&json)
            ));
        }

        Ok(cmd)
    }
}

#[async_trait]
impl SandboxProvider for ModalProvider {
    type Sandbox = DefaultSandbox;

    async fn prepare(
        &mut self,
        copy_dirs: &[(PathBuf, PathBuf)],
        _no_cache: bool,
        sandbox_init_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
        context_dir: Option<&std::path::Path>,
    ) -> ProviderResult<Option<String>> {
        let mut prepare_cmd = self.build_prepare_command(copy_dirs, sandbox_init_cmd);

        if let Some(dir) = context_dir {
            prepare_cmd.push_str(&format!(" --context-dir={}", dir.display()));
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
        let create_command = self.build_create_command(image_id)?;

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

    async fn prewarm_image_cache(
        &mut self,
        ctx: &crate::image_cache::PrewarmContext<'_>,
    ) -> anyhow::Result<crate::image_cache::PrewarmOutcome> {
        let outcome = crate::image_cache::run_prewarm_pipeline(self, ctx).await?;
        if let crate::image_cache::PrewarmOutcome::Resolved { ref image_id } = outcome {
            self.image_id = Some(image_id.clone());
        }
        Ok(outcome)
    }

    async fn prepare_from_checkpoint(
        &mut self,
        base_image_id: &str,
        patch_file: &std::path::Path,
        sandbox_project_root: &str,
        discovery_done: Option<&std::sync::atomic::AtomicBool>,
    ) -> ProviderResult<Option<String>> {
        let cmd = format!(
            "uv run @modal_sandbox.py prepare --from-base-image={} --patch-file={} --sandbox-project-root={}",
            base_image_id,
            patch_file.display(),
            sandbox_project_root
        );
        let image_id = run_prepare_command(&self.connector, &cmd, "Modal", discovery_done).await?;
        self.image_id = Some(image_id.clone());
        Ok(Some(image_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(config: ModalProviderConfig) -> ModalProvider {
        ModalProvider::from_config(config)
    }

    // -- prepare command tests --

    #[test]
    fn test_prepare_command_defaults() {
        let p = provider(ModalProviderConfig::default());
        let cmd = p.build_prepare_command(&[], None);
        assert_eq!(cmd, "uv run @modal_sandbox.py prepare");
    }

    #[test]
    fn test_prepare_command_with_dockerfile_and_include_cwd() {
        let p = provider(ModalProviderConfig {
            dockerfile: Some("./Dockerfile".to_string()),
            include_cwd: true,
            ..Default::default()
        });
        let cmd = p.build_prepare_command(&[], None);
        assert_eq!(
            cmd,
            "uv run @modal_sandbox.py prepare ./Dockerfile --include-cwd"
        );
    }

    #[test]
    fn test_prepare_command_with_copy_dirs() {
        let p = provider(ModalProviderConfig {
            copy_dirs: vec!["./src:/app/src".to_string()],
            ..Default::default()
        });
        let runtime_dirs = vec![(PathBuf::from("./tests"), PathBuf::from("/app/tests"))];
        let cmd = p.build_prepare_command(&runtime_dirs, None);
        assert_eq!(
            cmd,
            "uv run @modal_sandbox.py prepare \
             --copy-dir=./src:/app/src \
             --copy-dir=./tests:/app/tests"
        );
    }

    #[test]
    fn test_prepare_command_with_sandbox_init_cmd() {
        let p = provider(ModalProviderConfig::default());
        let cmd = p.build_prepare_command(&[], Some("apt-get update && apt-get install -y curl"));
        assert_eq!(
            cmd,
            "uv run @modal_sandbox.py prepare \
             --sandbox-init-cmd='apt-get update && apt-get install -y curl'"
        );
    }

    #[test]
    fn test_prepare_command_all_options() {
        let p = provider(ModalProviderConfig {
            dockerfile: Some("./Dockerfile.test".to_string()),
            include_cwd: true,
            copy_dirs: vec!["./src:/app/src".to_string()],
            ..Default::default()
        });
        let runtime_dirs = vec![(PathBuf::from("./tests"), PathBuf::from("/app/tests"))];
        let cmd = p.build_prepare_command(&runtime_dirs, Some("make setup"));
        assert_eq!(
            cmd,
            "uv run @modal_sandbox.py prepare ./Dockerfile.test --include-cwd \
             --copy-dir=./src:/app/src \
             --copy-dir=./tests:/app/tests \
             --sandbox-init-cmd='make setup'"
        );
    }

    // -- create command tests --

    #[test]
    fn test_create_command_without_experimental_options() -> ProviderResult<()> {
        let p = provider(ModalProviderConfig {
            cpu_cores: 0.125,
            ..Default::default()
        });
        let cmd = p.build_create_command("im-abc123")?;
        assert_eq!(cmd, "uv run @modal_sandbox.py create --cpu 0.125 im-abc123");
        assert!(!cmd.contains("--experimental-options"));
        Ok(())
    }

    #[test]
    fn test_create_command_custom_cpu() -> ProviderResult<()> {
        let p = provider(ModalProviderConfig {
            cpu_cores: 2.0,
            ..Default::default()
        });
        let cmd = p.build_create_command("im-xyz")?;
        assert_eq!(cmd, "uv run @modal_sandbox.py create --cpu 2 im-xyz");
        Ok(())
    }

    #[test]
    fn test_create_command_with_memory_gb() -> ProviderResult<()> {
        let p = provider(ModalProviderConfig {
            cpu_cores: 0.125,
            memory_gb: Some(8.0),
            ..Default::default()
        });
        let cmd = p.build_create_command("im-abc123")?;
        assert!(cmd.contains("--memory-gb 8"));
        assert!(cmd.contains("--cpu 0.125"));
        assert!(cmd.contains("im-abc123"));
        Ok(())
    }

    #[test]
    fn test_create_command_without_memory_gb() -> ProviderResult<()> {
        let p = provider(ModalProviderConfig {
            cpu_cores: 0.125,
            memory_gb: None,
            ..Default::default()
        });
        let cmd = p.build_create_command("im-abc123")?;
        assert!(!cmd.contains("--memory-gb"));
        Ok(())
    }

    #[test]
    fn test_create_command_with_experimental_options() -> ProviderResult<()> {
        let mut experimental_options = std::collections::HashMap::new();
        experimental_options.insert("enable_docker".to_string(), toml::Value::Boolean(true));

        let p = provider(ModalProviderConfig {
            experimental_options,
            cpu_cores: 0.125,
            ..Default::default()
        });
        let cmd = p.build_create_command("im-abc123")?;
        assert!(cmd.starts_with("uv run @modal_sandbox.py create --cpu 0.125 im-abc123"));
        assert!(cmd.contains("--experimental-options"));
        assert!(cmd.contains("enable_docker"));
        assert!(cmd.contains("true"));
        Ok(())
    }
}
