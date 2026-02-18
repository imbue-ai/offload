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
use tracing::{debug, warn};

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
            eprintln!("Preparing environment...");

            // Build prepare command with copy_dirs (both TOML-configured and CLI-provided)
            let mut full_prepare_cmd = prepare_cmd.clone();
            for copy_spec in &config.copy_dirs {
                full_prepare_cmd.push_str(&format!(" --copy-dir={}", copy_spec));
            }
            for (local, remote) in copy_dirs {
                full_prepare_cmd.push_str(&format!(
                    " --copy-dir={}:{}",
                    local.display(),
                    remote.display()
                ));
            }

            // Stream output in real-time
            use crate::provider::OutputLine;
            use futures::StreamExt;

            let mut stream = connector.run_stream(&full_prepare_cmd).await?;
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
                    "Prepare command failed with exit code {}",
                    exit_code
                )));
            }

            // Image id is the last line of stdout
            let image_id = last_stdout_line.trim().to_string();

            if image_id.is_empty() {
                return Err(ProviderError::ExecFailed(
                    "Prepare command returned empty image_id".to_string(),
                ));
            }

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
        debug!("Creating default sandbox: {}", config.id);

        // Build the create command, substituting {image_id} if available
        // Note: copy_dirs are already baked into the image during prepare
        let create_command = match self.image_id.as_ref() {
            Some(id) => self.config.create_command.replace("{image_id}", id),
            None => self.config.create_command.clone(),
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

        Ok(DefaultSandbox {
            id: config.id.clone(),
            remote_id,
            connector: self.connector.clone(),
            exec_command: self.config.exec_command.clone(),
            destroy_command: self.config.destroy_command.clone(),
            download_command: self.config.download_command.clone(),
            env: self.base_env(),
        })
    }

    fn base_env(&self) -> Vec<(String, String)> {
        self.config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
    /// Environment variables to pass to commands
    env: Vec<(String, String)>,
}

impl DefaultSandbox {
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

        // Combine env vars and command
        let inner_cmd = if env_prefix.is_empty() {
            program_and_args
        } else {
            format!("{} {}", env_prefix, program_and_args)
        };

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
                debug!("Downloaded {} -> {}", remote, local);
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    async fn terminate(&self) -> ProviderResult<()> {
        let shell_cmd = self.build_destroy_command();
        debug!(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a DefaultSandbox with given env vars for testing.
    fn sandbox_with_env(env: Vec<(String, String)>) -> DefaultSandbox {
        DefaultSandbox {
            id: "test-sandbox".to_string(),
            remote_id: "remote-123".to_string(),
            connector: Arc::new(ShellConnector::new()),
            exec_command: "exec --sandbox {sandbox_id} --cmd {command}".to_string(),
            destroy_command: "destroy {sandbox_id}".to_string(),
            download_command: None,
            env,
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

        // The sandbox_id placeholder should be replaced with the remote_id
        assert!(
            result.contains("remote-123"),
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

        // Should have FOO=bar prefix before the command
        assert!(
            result.contains("FOO=bar"),
            "result should contain env var: {result}"
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

        // Both env vars should be present
        assert!(
            result.contains("FOO=bar"),
            "result should contain first env var: {result}"
        );
        assert!(
            result.contains("BAZ=qux"),
            "result should contain second env var: {result}"
        );
        // They should be space-separated in the prefix
        assert!(
            result.contains("FOO=bar BAZ=qux") || result.contains("BAZ=qux FOO=bar"),
            "env vars should be space-separated: {result}"
        );
    }

    #[test]
    fn test_build_exec_command_env_var_with_spaces() {
        let sandbox = sandbox_with_env(vec![("MESSAGE".to_string(), "hello world".to_string())]);
        let command = cmd("echo", &[]);

        let result = sandbox.build_exec_command(&command);

        // Value with spaces should be quoted. The inner command is then escaped again,
        // so 'hello world' becomes '\''hello world'\'' in the final output
        // We verify that MESSAGE= is present and the command template is filled
        assert!(
            result.contains("MESSAGE="),
            "env var name should be present: {result}"
        );
        // The value "hello world" should appear somewhere in the result (possibly escaped)
        assert!(
            result.contains("hello world"),
            "env var value should be present (possibly escaped): {result}"
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

        // {sandbox_id} should be replaced with the remote_id
        assert!(
            result.contains("remote-123"),
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
}
