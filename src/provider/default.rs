//! Default provider — uses custom shell commands for sandbox lifecycle management.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use async_trait::async_trait;
use tracing::{debug, warn};

use super::{
    Command, CostEstimate, OutputStream, ProviderError, ProviderResult, Sandbox, SandboxProvider,
    run_prepare_command,
};

/// Modal non-preemptible pricing: $0.00003942 per CPU-core per second.
const MODAL_CPU_COST_PER_CORE_PER_SEC: f64 = 0.00003942;
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
/// If `prepare_command` is configured, calling `prepare()` runs it and
/// stores the resulting image ID. This image ID is then substituted into
/// `create_command` via the `{image_id}` placeholder.
pub struct DefaultProvider {
    connector: Arc<ShellConnector>,
    config: DefaultProviderConfig,
    /// Set during `prepare()`.
    image_id: Option<String>,
}

impl DefaultProvider {
    /// Creates a new provider from the given configuration.
    ///
    /// This is a lightweight constructor that stores the config and creates
    /// the shell connector. No I/O is performed. Call
    /// [`prepare()`](SandboxProvider::prepare) to run the image build.
    pub fn from_config(config: DefaultProviderConfig) -> Self {
        let mut connector = ShellConnector::new().with_timeout(config.timeout_secs);

        if let Some(dir) = &config.working_dir {
            connector = connector.with_working_dir(dir.clone());
        }

        let connector = Arc::new(connector);

        Self {
            connector,
            config,
            image_id: None,
        }
    }

    /// Builds the full prepare command string, or `None` if no `prepare_command` is configured.
    fn build_prepare_command(
        &self,
        copy_dirs: &[(std::path::PathBuf, std::path::PathBuf)],
        sandbox_init_cmd: Option<&str>,
        context_dir: Option<&std::path::Path>,
    ) -> Option<String> {
        let prepare_cmd = self.config.prepare_command.as_ref()?;
        let mut full = prepare_cmd.clone();

        for copy_spec in &self.config.copy_dirs {
            full.push_str(&format!(" --copy-dir={}", copy_spec));
        }
        for (local, remote) in copy_dirs {
            full.push_str(&format!(
                " --copy-dir={}:{}",
                local.display(),
                remote.display()
            ));
        }

        if let Some(init_cmd) = sandbox_init_cmd {
            full.push_str(&format!(
                " --sandbox-init-cmd={}",
                shell_words::quote(init_cmd)
            ));
        }

        if let Some(dir) = context_dir {
            full.push_str(&format!(" --context-dir={}", dir.display()));
        }

        Some(full)
    }
}

#[async_trait]
impl SandboxProvider for DefaultProvider {
    type Sandbox = DefaultSandbox;

    async fn prepare(
        &mut self,
        copy_dirs: &[(std::path::PathBuf, std::path::PathBuf)],
        _no_cache: bool,
        sandbox_init_cmd: Option<&str>,
        discovery_done: Option<&AtomicBool>,
        context_dir: Option<&std::path::Path>,
    ) -> ProviderResult<Option<String>> {
        let image_id = if let Some(full_prepare_cmd) =
            self.build_prepare_command(copy_dirs, sandbox_init_cmd, context_dir)
        {
            let image_id = run_prepare_command(
                &self.connector,
                &full_prepare_cmd,
                "Default",
                discovery_done,
            )
            .await?;

            Some(image_id)
        } else {
            None
        };

        self.image_id = image_id.clone();
        Ok(image_id)
    }

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DefaultSandbox> {
        debug!("Creating default sandbox: {}", config.id);

        let cpu_cores = self.config.cpu_cores;
        let cpu_cores_str = cpu_cores.to_string();

        // Build the create command, substituting {image_id} and {cpu_cores} if available
        // Note: copy_dirs are already baked into the image during prepare
        let create_command = match self.image_id.as_ref() {
            Some(id) => self
                .config
                .create_command
                .replace("{image_id}", id)
                .replace("{cpu_cores}", &cpu_cores_str),
            None => self
                .config
                .create_command
                .replace("{cpu_cores}", &cpu_cores_str),
        };

        debug!("{}", create_command);

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

        debug!("Created default sandbox with ID: {}", remote_id);

        // Merge provider base env with sandbox-specific env (includes OFFLOAD_ROOT)
        let mut env = self.base_env();
        env.extend(config.env.iter().cloned());

        // Apply {cpu_cores} placeholder to command templates
        let exec_command = self
            .config
            .exec_command
            .replace("{cpu_cores}", &cpu_cores_str);
        let destroy_command = self
            .config
            .destroy_command
            .replace("{cpu_cores}", &cpu_cores_str);
        let download_command = self
            .config
            .download_command
            .as_ref()
            .map(|cmd| cmd.replace("{cpu_cores}", &cpu_cores_str));

        Ok(DefaultSandbox {
            id: remote_id,
            connector: self.connector.clone(),
            exec_command,
            destroy_command,
            download_command,
            env,
            created_at: Instant::now(),
            cpu_cores,
        })
    }

    fn base_env(&self) -> Vec<(String, String)> {
        self.config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
        let image_id =
            run_prepare_command(&self.connector, &cmd, "Default", discovery_done).await?;
        self.image_id = Some(image_id.clone());
        Ok(Some(image_id))
    }
}

/// A sandbox managed through shell command templates.
///
/// The sandbox maintains an `id` (returned by the create command)
/// that is substituted into the exec and destroy command templates.
///
/// # Reusability
///
/// Unlike single-use sandboxes, this sandbox can execute multiple commands
/// on the same remote instance. This is useful for stateful workflows where
/// subsequent commands depend on previous ones.
///
/// # File Download
///
/// File download is supported via an optional `download_command` template.
/// Files should be included in the execution environment at build time
/// (e.g., baked into a container image).
///
pub struct DefaultSandbox {
    id: String,
    connector: Arc<ShellConnector>,
    exec_command: String,
    destroy_command: String,
    download_command: Option<String>,
    env: Vec<(String, String)>,
    created_at: Instant,
    cpu_cores: f64,
}

impl DefaultSandbox {
    /// Creates a new DefaultSandbox. Used by providers that create
    /// sandboxes with custom command templates (e.g., ModalProvider).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        connector: Arc<ShellConnector>,
        exec_command: String,
        destroy_command: String,
        download_command: Option<String>,
        env: Vec<(String, String)>,
        created_at: Instant,
        cpu_cores: f64,
    ) -> Self {
        Self {
            id,
            connector,
            exec_command,
            destroy_command,
            download_command,
            env,
            created_at,
            cpu_cores,
        }
    }

    /// Build the exec command with substitutions.
    fn build_exec_command(&self, cmd: &Command) -> String {
        // Build env var prefix (KEY=value KEY2=value2 ...)
        let env_prefix = self
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, shell_words::quote(v)))
            .collect::<Vec<_>>()
            .join(" ");

        // Build the inner command with properly escaped arguments
        let program_and_args = std::iter::once(cmd.program.as_str())
            .chain(cmd.args.iter().map(|s| s.as_str()))
            .map(|a| shell_words::quote(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        // Combine env prefix and command
        let inner_cmd = if env_prefix.is_empty() {
            program_and_args
        } else {
            format!("{} {}", env_prefix, program_and_args)
        };

        // Prepend cd to project root if OFFLOAD_ROOT is set
        let inner_cmd = match self.env.iter().find(|(k, _)| k == "OFFLOAD_ROOT") {
            Some((_, root)) => format!("cd {} && {}", shell_words::quote(root), inner_cmd),
            None => inner_cmd,
        };

        // Escape the entire command so it can be passed as a single shell argument
        let escaped_cmd = shell_words::quote(&inner_cmd);

        self.exec_command
            .replace("{sandbox_id}", &self.id)
            .replace("{command}", &escaped_cmd)
    }

    /// Build the destroy command with substitutions.
    fn build_destroy_command(&self) -> String {
        self.destroy_command.replace("{sandbox_id}", &self.id)
    }

    /// Build the download command with substitutions.
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

            cmd.replace("{sandbox_id}", &self.id)
                .replace("{paths}", &paths_str)
        })
    }
}

#[async_trait]
impl Sandbox for DefaultSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec_stream(
        &mut self,
        cmd: &Command,
    ) -> ProviderResult<(OutputStream, tokio::process::Child)> {
        let shell_cmd = self.build_exec_command(cmd);
        debug!("Streaming on {}: {}", self.id, shell_cmd);
        self.connector.run_stream_with_child(&shell_cmd).await
    }

    async fn download(&mut self, paths: &[(&Path, &Path)]) -> ProviderResult<()> {
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
            debug!("Downloading from {}: {} path(s)", self.id, paths.len());
            let result = self.connector.run(&shell_cmd).await?;

            if result.exit_code != 0 {
                return Err(ProviderError::DownloadFailed(format!(
                    "Download command failed: {}",
                    result.stderr
                )));
            }

            for (remote, local) in &path_pairs {
                debug!("Downloaded {} -> {}", remote, local);
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    async fn terminate(self) -> ProviderResult<()> {
        let shell_cmd = self.build_destroy_command();
        debug!("Terminating sandbox {}", self.id);

        let result = self.connector.run(&shell_cmd).await?;

        if result.exit_code != 0 {
            warn!("Destroy command failed: {}", result.stderr);
        }

        Ok(())
    }

    fn cost_estimate(&self) -> CostEstimate {
        let elapsed = self.created_at.elapsed().as_secs_f64();
        let cpu_seconds = elapsed * self.cpu_cores;
        let estimated_cost_usd = cpu_seconds * MODAL_CPU_COST_PER_CORE_PER_SEC;
        CostEstimate {
            cpu_seconds,
            estimated_cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a DefaultSandbox with given env vars for testing.
    fn sandbox_with_env(env: Vec<(String, String)>) -> DefaultSandbox {
        DefaultSandbox {
            id: "sb-test-123".to_string(),
            connector: Arc::new(ShellConnector::new()),
            exec_command: "exec --sandbox {sandbox_id} --cmd {command}".to_string(),
            destroy_command: "destroy {sandbox_id}".to_string(),
            download_command: None,
            env,
            created_at: Instant::now(),
            cpu_cores: 1.0,
        }
    }

    /// Creates a Command with the given program and args.
    fn cmd(program: &str, args: &[&str]) -> Command {
        Command {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            working_dir: None,
            env: Vec::new(),
            timeout_secs: None,
        }
    }

    #[test]
    fn test_build_exec_command_no_env_vars() {
        let sandbox = sandbox_with_env(vec![]);
        let command = cmd("pytest", &["test_foo.py", "-v"]);

        let result = sandbox.build_exec_command(&command);

        // The sandbox_id placeholder should be replaced with the id
        assert!(
            result.contains("sb-test-123"),
            "sandbox_id should be substituted: {result}"
        );
        assert!(
            !result.contains("{sandbox_id}"),
            "sandbox_id placeholder should be replaced: {result}"
        );
        // Program and args should be present (properly escaped)
        assert!(
            result.contains("pytest"),
            "command should contain program: {result}"
        );
        assert!(
            result.contains("test_foo.py"),
            "command should contain first arg: {result}"
        );
        assert!(
            result.contains("-v"),
            "command should contain second arg: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_single_env_var() {
        let sandbox = sandbox_with_env(vec![("FOO".to_string(), "bar".to_string())]);
        let command = cmd("echo", &["hello"]);

        let result = sandbox.build_exec_command(&command);

        // Should have FOO=bar as env prefix before the command
        assert!(
            result.contains("FOO=bar"),
            "result should contain env var prefix: {result}"
        );
        // No OFFLOAD_ROOT in env, so no cd should be prepended
        assert!(
            !result.contains("cd "),
            "result should not contain cd without OFFLOAD_ROOT: {result}"
        );
        assert!(result.contains("echo"), "result should contain program");
    }

    #[test]
    fn test_build_exec_command_multiple_env_vars() {
        let sandbox = sandbox_with_env(vec![
            ("FOO".to_string(), "bar".to_string()),
            ("BAZ".to_string(), "qux".to_string()),
        ]);
        let command = cmd("myprogram", &[]);

        let result = sandbox.build_exec_command(&command);

        // Both env vars should be present as prefix
        assert!(
            result.contains("FOO=bar"),
            "result should contain first env var prefix: {result}"
        );
        assert!(
            result.contains("BAZ=qux"),
            "result should contain second env var prefix: {result}"
        );
        // No OFFLOAD_ROOT in env, so no cd should be prepended
        assert!(
            !result.contains("cd "),
            "result should not contain cd without OFFLOAD_ROOT: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_env_var_with_spaces() {
        let sandbox = sandbox_with_env(vec![("MESSAGE".to_string(), "hello world".to_string())]);
        let command = cmd("echo", &[]);

        let result = sandbox.build_exec_command(&command);

        // Value with spaces should be quoted as env prefix
        assert!(
            result.contains("MESSAGE="),
            "env var name should be present: {result}"
        );
        // The value "hello world" should appear somewhere in the result (possibly escaped)
        assert!(
            result.contains("hello world"),
            "env var value should be present (possibly escaped): {result}"
        );
        // No OFFLOAD_ROOT in env, so no cd should be prepended
        assert!(
            !result.contains("cd "),
            "result should not contain cd without OFFLOAD_ROOT: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_env_var_with_quotes() {
        let sandbox = sandbox_with_env(vec![("QUOTED".to_string(), "it's \"quoted\"".to_string())]);
        let command = cmd("echo", &[]);

        let result = sandbox.build_exec_command(&command);

        // Value should be properly shell-quoted to handle quotes
        // shell_words::quote will use single quotes and escape internal single quotes
        assert!(
            result.contains("QUOTED="),
            "result should contain env var name: {result}"
        );
        // The value should be escaped - shell_words uses single quotes for strings with special chars
        // and doubles single quotes inside, so "it's" becomes "'it'\\''s \"quoted\"'"
        // We just verify it's not the raw unescaped value
        assert!(
            !result.contains("QUOTED=it's"),
            "value with quotes should not appear unescaped: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_env_var_empty_value() {
        let sandbox = sandbox_with_env(vec![("EMPTY".to_string(), String::new())]);
        let command = cmd("echo", &[]);

        let result = sandbox.build_exec_command(&command);

        // Empty value should be properly quoted. The inner command is then escaped again.
        // shell_words::quote("") returns "''" and when the whole command is quoted,
        // the inner '' becomes '\'''\'' in the final output
        assert!(
            result.contains("EMPTY="),
            "env var name should be present: {result}"
        );
        // The command template should be filled
        assert!(
            !result.contains("{command}"),
            "command placeholder should be replaced: {result}"
        );
        // The result should contain the escaped empty quotes somewhere
        // This verifies the empty value was handled (not omitted)
        assert!(
            result.contains("EMPTY='\\''"),
            "empty value should be escaped in the final command: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_sandbox_id_substitution() {
        let sandbox = sandbox_with_env(vec![]);
        let command = cmd("test", &[]);

        let result = sandbox.build_exec_command(&command);

        // {sandbox_id} should be replaced with the id
        assert!(
            result.contains("sb-test-123"),
            "sandbox_id should be substituted: {result}"
        );
        assert!(
            !result.contains("{sandbox_id}"),
            "placeholder should be replaced: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_command_substitution() {
        let sandbox = sandbox_with_env(vec![]);
        let command = cmd("pytest", &["--verbose"]);

        let result = sandbox.build_exec_command(&command);

        // {command} should be replaced with the escaped inner command
        assert!(
            !result.contains("{command}"),
            "command placeholder should be replaced: {result}"
        );
        // The actual command should be present (escaped)
        assert!(
            result.contains("pytest"),
            "program should be in result: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_args_with_special_chars() {
        let sandbox = sandbox_with_env(vec![]);
        let command = cmd("echo", &["hello world", "foo'bar"]);

        let result = sandbox.build_exec_command(&command);

        // Arguments with special characters should be properly escaped
        // shell_words::quote will quote strings with spaces
        assert!(
            result.contains("'hello world'"),
            "arg with space should be quoted: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_offload_root_cd() {
        let sandbox = sandbox_with_env(vec![
            ("OFFLOAD_ROOT".to_string(), "/code/mng".to_string()),
            ("FOO".to_string(), "bar".to_string()),
        ]);
        let command = cmd("pytest", &["-v"]);

        let result = sandbox.build_exec_command(&command);

        // cd with literal OFFLOAD_ROOT path should be prepended
        assert!(
            result.contains("cd /code/mng"),
            "result should contain cd with literal OFFLOAD_ROOT path: {result}"
        );
        // OFFLOAD_ROOT should appear as env prefix
        assert!(
            result.contains("OFFLOAD_ROOT="),
            "result should contain OFFLOAD_ROOT env var: {result}"
        );
        // FOO=bar should appear as env prefix
        assert!(
            result.contains("FOO=bar"),
            "result should contain FOO env var prefix: {result}"
        );
        // Program should be present
        assert!(
            result.contains("pytest"),
            "result should contain program: {result}"
        );
        // cd should come before the env prefix (cd is prepended to the whole inner command)
        let cd_pos = result.find("cd /code/mng");
        let env_pos = result.find("OFFLOAD_ROOT=");
        assert!(
            cd_pos < env_pos,
            "cd should appear before env prefix: {result}"
        );
    }

    #[test]
    fn cost_estimate_scales_with_cpu_cores() {
        use crate::provider::Sandbox;

        let sandbox = DefaultSandbox {
            id: "sb-cost-1".to_string(),
            connector: Arc::new(ShellConnector::new()),
            exec_command: String::new(),
            destroy_command: String::new(),
            download_command: None,
            env: vec![],
            created_at: Instant::now() - std::time::Duration::from_secs(100),
            cpu_cores: 2.0,
        };

        let cost = sandbox.cost_estimate();
        // 100s * 2.0 cores = ~200 CPU-seconds (allow small timing tolerance)
        assert!(
            cost.cpu_seconds >= 199.0 && cost.cpu_seconds <= 201.0,
            "cpu_seconds should be ~200: {}",
            cost.cpu_seconds
        );
        let expected_usd = cost.cpu_seconds * MODAL_CPU_COST_PER_CORE_PER_SEC;
        assert!(
            (cost.estimated_cost_usd - expected_usd).abs() < 0.0001,
            "cost should match rate * cpu_seconds"
        );
    }

    #[test]
    fn cost_estimate_fractional_cpu_cores() {
        use crate::provider::Sandbox;

        let sandbox = DefaultSandbox {
            id: "sb-cost-2".to_string(),
            connector: Arc::new(ShellConnector::new()),
            exec_command: String::new(),
            destroy_command: String::new(),
            download_command: None,
            env: vec![],
            created_at: Instant::now() - std::time::Duration::from_secs(100),
            cpu_cores: 0.125,
        };

        let cost = sandbox.cost_estimate();
        // 100s * 0.125 cores = ~12.5 CPU-seconds
        assert!(
            cost.cpu_seconds >= 12.0 && cost.cpu_seconds <= 13.0,
            "cpu_seconds should be ~12.5: {}",
            cost.cpu_seconds
        );
        assert!(
            cost.estimated_cost_usd > 0.0,
            "cost should be positive for remote sandboxes"
        );
    }

    /// Integration test for Modal sandbox download functionality via DefaultProvider.
    ///
    /// This test requires Modal credentials (MODAL_TOKEN_ID and MODAL_TOKEN_SECRET).
    /// Skips automatically if credentials are not present.
    #[tokio::test]
    async fn modal_download_junit_xml() -> Result<(), Box<dyn std::error::Error>> {
        use crate::config::{DefaultProviderConfig, SandboxConfig};
        use crate::provider::SandboxProvider;
        use futures::StreamExt;

        // Skip if Modal credentials are not available
        if std::env::var("MODAL_TOKEN_ID").is_err() || std::env::var("MODAL_TOKEN_SECRET").is_err()
        {
            eprintln!(
                "Skipping modal_download_junit_xml: MODAL_TOKEN_ID or MODAL_TOKEN_SECRET not set"
            );
            return Ok(());
        }

        // Create a temp dir with a minimal Dockerfile
        let temp_dir = tempfile::tempdir()?;
        let dockerfile_path = temp_dir.path().join("Dockerfile.test");
        std::fs::write(&dockerfile_path, "FROM python:3.11-slim\n")?;

        // Configure DefaultProvider to use modal_sandbox.py
        // Use @modal_sandbox.py notation which resolves to the bundled script
        let config = DefaultProviderConfig {
            prepare_command: Some("uv run @modal_sandbox.py prepare Dockerfile.test".to_string()),
            create_command: "uv run @modal_sandbox.py create {image_id}".to_string(),
            exec_command: "uv run @modal_sandbox.py exec {sandbox_id} {command}".to_string(),
            destroy_command: "uv run @modal_sandbox.py destroy {sandbox_id}".to_string(),
            download_command: Some(
                "uv run @modal_sandbox.py download {sandbox_id} {paths}".to_string(),
            ),
            working_dir: Some(temp_dir.path().to_path_buf()),
            timeout_secs: 300,
            env: Default::default(),
            copy_dirs: vec![],
            cpu_cores: 1.0,
        };

        let mut provider = DefaultProvider::from_config(config);
        provider.prepare(&[], false, None, None, None).await?;

        // Create sandbox
        let sandbox_config = SandboxConfig {
            id: "test-download".to_string(),
            working_dir: None,
            env: vec![],
            copy_dirs: vec![],
        };

        let mut sandbox = provider.create_sandbox(&sandbox_config).await?;

        // Write test junit.xml content
        let test_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="test_suite" tests="2" failures="0">
    <testcase name="test_one" classname="tests.test_example" time="0.001"/>
    <testcase name="test_two" classname="tests.test_example" time="0.002"/>
  </testsuite>
</testsuites>"#;

        // Write the file to sandbox using exec
        let write_cmd = Command::new("sh").arg("-c").arg(format!(
            "cat > /tmp/junit.xml << 'EOF'\n{}\nEOF",
            test_content
        ));

        let (mut stream, _child) = sandbox.exec_stream(&write_cmd).await?;
        while stream.next().await.is_some() {}

        // Download the file
        let download_dir = tempfile::tempdir()?;
        let local_file = download_dir.path().join("downloaded.xml");
        let remote_path = std::path::Path::new("/tmp/junit.xml");

        sandbox
            .download(&[(remote_path, local_file.as_path())])
            .await?;

        // Verify content
        let downloaded = std::fs::read_to_string(&local_file)?;
        assert_eq!(
            test_content.trim(),
            downloaded.trim(),
            "Downloaded content should match"
        );

        // Cleanup
        sandbox.terminate().await?;

        Ok(())
    }
}
