//! Docker container provider implementation.
//!
//! This provider runs tests in Docker containers, providing isolation
//! and reproducibility.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, LogOutput,
    RemoveContainerOptions, StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::Docker;
use futures::StreamExt;
use tokio::sync::Mutex;

use super::{
    Command, DynSandbox, ExecResult, OutputStream, OutputLine, ProviderError, ProviderResult, Sandbox,
    SandboxInfo, SandboxProvider, SandboxStatus,
};
use crate::config::{DockerProviderConfig, SandboxConfig};

/// Docker container provider.
pub struct DockerProvider {
    docker: Docker,
    config: DockerProviderConfig,
    containers: Arc<Mutex<HashMap<String, ContainerInfo>>>,
}

#[derive(Clone)]
struct ContainerInfo {
    #[allow(dead_code)]
    container_id: String,
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl DockerProvider {
    /// Create a new Docker provider with the given configuration.
    pub fn new(config: DockerProviderConfig) -> ProviderResult<Self> {
        let docker = if let Some(host) = &config.docker_host {
            Docker::connect_with_http(host, 120, bollard::API_DEFAULT_VERSION)
                .map_err(|e| ProviderError::Connection(e.to_string()))?
        } else {
            Docker::connect_with_local_defaults()
                .map_err(|e| ProviderError::Connection(e.to_string()))?
        };

        Ok(Self {
            docker,
            config,
            containers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Create a new Docker provider, connecting to the local Docker daemon.
    pub fn local(config: DockerProviderConfig) -> ProviderResult<Self> {
        Self::new(config)
    }
}

#[async_trait]
impl SandboxProvider for DockerProvider {
    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<DynSandbox> {
        // Build environment variables
        let mut env: Vec<String> = self.config.env.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        for (k, v) in &config.env {
            env.push(format!("{}={}", k, v));
        }

        // Build volume bindings
        let binds: Vec<String> = self.config.volumes.clone();

        // Build host config
        let mut host_config = bollard::models::HostConfig {
            binds: Some(binds),
            network_mode: Some(self.config.network_mode.clone()),
            ..Default::default()
        };

        // Set resource limits
        if let Some(cpu) = self.config.resources.cpu_limit {
            // CPU period in microseconds
            host_config.cpu_period = Some(100_000);
            // CPU quota (cpu_limit * cpu_period)
            host_config.cpu_quota = Some((cpu * 100_000.0) as i64);
        }

        if let Some(memory) = self.config.resources.memory_limit {
            host_config.memory = Some(memory);
        }

        if let Some(memory_swap) = self.config.resources.memory_swap {
            host_config.memory_swap = Some(memory_swap);
        }

        // Create container config
        let working_dir = config.working_dir.as_ref()
            .or(self.config.working_dir.as_ref())
            .map(|s| s.to_string());

        let container_config = ContainerConfig {
            image: Some(self.config.image.clone()),
            env: Some(env),
            working_dir,
            host_config: Some(host_config),
            // Keep container running
            cmd: Some(vec!["sleep".to_string(), "infinity".to_string()]),
            tty: Some(true),
            ..Default::default()
        };

        // Create the container
        let options = CreateContainerOptions {
            name: &config.id,
            platform: None,
        };

        let response = self.docker
            .create_container(Some(options), container_config)
            .await
            .map_err(|e| ProviderError::CreateFailed(e.to_string()))?;

        let container_id = response.id;

        // Start the container
        self.docker
            .start_container(&container_id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| ProviderError::CreateFailed(e.to_string()))?;

        // Track the container
        let info = ContainerInfo {
            container_id: container_id.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.containers.lock().await.insert(config.id.clone(), info);

        Ok(Box::new(DockerSandbox {
            id: config.id.clone(),
            container_id,
            docker: self.docker.clone(),
            working_dir: self.config.working_dir.clone(),
        }))
    }

    async fn list_sandboxes(&self) -> ProviderResult<Vec<SandboxInfo>> {
        let containers = self.containers.lock().await;
        Ok(containers
            .iter()
            .map(|(id, info)| SandboxInfo {
                id: id.clone(),
                status: info.status,
                created_at: info.created_at,
            })
            .collect())
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}

/// A sandbox backed by a Docker container.
pub struct DockerSandbox {
    id: String,
    container_id: String,
    docker: Docker,
    working_dir: Option<String>,
}

#[async_trait]
impl Sandbox for DockerSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, cmd: &Command) -> ProviderResult<ExecResult> {
        let start = Instant::now();

        // Build the command
        let shell_cmd = cmd.to_shell_string();
        let exec_cmd = vec!["/bin/sh".to_string(), "-c".to_string(), shell_cmd];

        // Build environment
        let env: Vec<String> = cmd.env.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let working_dir = cmd.working_dir.as_ref()
            .or(self.working_dir.as_ref())
            .cloned();

        // Create exec instance
        let exec_options = CreateExecOptions {
            cmd: Some(exec_cmd),
            env: Some(env),
            working_dir,
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec = self.docker
            .create_exec(&self.container_id, exec_options)
            .await
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        // Start exec and collect output
        let output = self.docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        if let StartExecResults::Attached { mut output, .. } = output {
            while let Some(msg) = output.next().await {
                match msg {
                    Ok(LogOutput::StdOut { message }) => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(LogOutput::StdErr { message }) => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        // Get exit code
        let inspect = self.docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        let exit_code = inspect.exit_code.unwrap_or(-1) as i32;
        let duration = start.elapsed();

        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
            duration,
        })
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let shell_cmd = cmd.to_shell_string();
        let exec_cmd = vec!["/bin/sh".to_string(), "-c".to_string(), shell_cmd];

        let env: Vec<String> = cmd.env.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let working_dir = cmd.working_dir.as_ref()
            .or(self.working_dir.as_ref())
            .cloned();

        let exec_options = CreateExecOptions {
            cmd: Some(exec_cmd),
            env: Some(env),
            working_dir,
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec = self.docker
            .create_exec(&self.container_id, exec_options)
            .await
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        let output = self.docker
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| ProviderError::ExecFailed(e.to_string()))?;

        if let StartExecResults::Attached { output, .. } = output {
            let stream = output.filter_map(|msg| async {
                match msg {
                    Ok(LogOutput::StdOut { message }) => {
                        Some(OutputLine::Stdout(String::from_utf8_lossy(&message).to_string()))
                    }
                    Ok(LogOutput::StdErr { message }) => {
                        Some(OutputLine::Stderr(String::from_utf8_lossy(&message).to_string()))
                    }
                    _ => None,
                }
            });
            Ok(Box::pin(stream))
        } else {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    async fn upload(&self, local: &Path, remote: &Path) -> ProviderResult<()> {
        // Use docker cp (via tar archive)
        let tar_data = create_tar_archive(local)
            .map_err(|e| ProviderError::UploadFailed(e.to_string()))?;

        let remote_dir = remote.parent().unwrap_or(Path::new("/"));

        self.docker
            .upload_to_container(
                &self.container_id,
                Some(bollard::container::UploadToContainerOptions {
                    path: remote_dir.to_string_lossy().to_string(),
                    ..Default::default()
                }),
                tar_data.into(),
            )
            .await
            .map_err(|e| ProviderError::UploadFailed(e.to_string()))?;

        Ok(())
    }

    async fn download(&self, remote: &Path, local: &Path) -> ProviderResult<()> {
        let mut stream = self.docker
            .download_from_container(
                &self.container_id,
                Some(bollard::container::DownloadFromContainerOptions {
                    path: remote.to_string_lossy().to_string(),
                }),
            );

        let mut tar_data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;
            tar_data.extend_from_slice(&chunk);
        }

        extract_tar_archive(&tar_data, local)
            .map_err(|e| ProviderError::DownloadFailed(e.to_string()))?;

        Ok(())
    }

    async fn status(&self) -> ProviderResult<SandboxStatus> {
        let info = self.docker
            .inspect_container(&self.container_id, None)
            .await
            .map_err(|e| ProviderError::NotFound(e.to_string()))?;

        let status = match info.state.and_then(|s| s.status) {
            Some(bollard::models::ContainerStateStatusEnum::RUNNING) => SandboxStatus::Running,
            Some(bollard::models::ContainerStateStatusEnum::CREATED) => SandboxStatus::Creating,
            Some(bollard::models::ContainerStateStatusEnum::EXITED) => SandboxStatus::Stopped,
            Some(bollard::models::ContainerStateStatusEnum::DEAD) => SandboxStatus::Failed,
            _ => SandboxStatus::Failed,
        };

        Ok(status)
    }

    async fn terminate(&self) -> ProviderResult<()> {
        self.docker
            .remove_container(
                &self.container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| ProviderError::Other(e.into()))?;

        Ok(())
    }
}

/// Create a tar archive from a file or directory.
fn create_tar_archive(path: &Path) -> std::io::Result<Vec<u8>> {
    

    let mut archive = tar::Builder::new(Vec::new());

    if path.is_dir() {
        archive.append_dir_all(path.file_name().unwrap_or_default(), path)?;
    } else {
        let mut file = std::fs::File::open(path)?;
        archive.append_file(path.file_name().unwrap_or_default(), &mut file)?;
    }

    archive.into_inner()
}

/// Extract a tar archive to a destination path.
fn extract_tar_archive(data: &[u8], dest: &Path) -> std::io::Result<()> {
    let mut archive = tar::Archive::new(data);

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    archive.unpack(dest)?;
    Ok(())
}
