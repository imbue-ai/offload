//! Modal cloud provider with image caching.
//!
//! This provider integrates directly with Modal cloud sandboxes.
//!
//! We persist a cache of Dockerfile hashes to the local disk to avoid rebuilding the Modal Sandbox Image.
//!
//! # Cache Keys
//!
//! - Dockerfile: `dockerfile:{path}` with hash validation
//! - Preset: `preset:{name}` (e.g., `preset:default`, `preset:rust`)
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, info, warn};

use super::{
    Command, OutputStream, ProviderError, ProviderResult, Sandbox, SandboxInfo, SandboxProvider,
    SandboxStatus,
};
use crate::cache::{ImageCache, ImageCacheEntry, compute_file_hash};
use crate::config::{ModalImageType, ModalProviderConfig, SandboxConfig};
use crate::connector::{Connector, ShellConnector};

/// Provider for Modal cloud sandboxes with image caching.
pub struct ModalProvider {
    config: ModalProviderConfig,
    connector: Arc<ShellConnector>,
    cache: Mutex<ImageCache>,
    cache_dir: PathBuf,
    sandboxes: Mutex<HashMap<String, ModalSandboxInfo>>,
    /// Tracks in-progress image builds to avoid duplicate builds.
    /// Maps cache_key -> OnceCell that will hold the image_id once built.
    image_builds: Mutex<HashMap<String, Arc<OnceCell<String>>>>,
}

#[allow(dead_code)]
struct ModalSandboxInfo {
    remote_id: String,
    status: SandboxStatus,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl ModalProvider {
    /// Creates a new Modal provider from configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Modal provider configuration
    /// * `working_dir` - Optional working directory for cache storage
    ///
    /// # Returns
    ///
    /// Result containing the provider instance, or an error if the working
    /// directory cannot be determined.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::CreateFailed` if:
    /// - Working directory cannot be determined
    /// - Working directory is not accessible
    pub fn from_config(
        config: ModalProviderConfig,
        working_dir: Option<PathBuf>,
    ) -> ProviderResult<Self> {
        // Determine cache directory
        let cache_dir = match working_dir {
            Some(d) => d,
            None => std::env::current_dir().map_err(|e| {
                ProviderError::CreateFailed(format!("Cannot determine working directory: {}", e))
            })?,
        };

        info!("Loading Modal image cache from: {}", cache_dir.display());
        let cache = ImageCache::load(&cache_dir);

        let connector = ShellConnector::new().with_timeout(config.timeout_secs);

        Ok(Self {
            config,
            connector: Arc::new(connector),
            cache: Mutex::new(cache),
            cache_dir,
            sandboxes: Mutex::new(HashMap::new()),
            image_builds: Mutex::new(HashMap::new()),
        })
    }

    /// Gets the cache key for the current image configuration.
    fn get_cache_key(&self) -> String {
        match &self.config.image_type {
            ModalImageType::Dockerfile { dockerfile } => format!("dockerfile:{}", dockerfile),
            ModalImageType::Preset { preset } => format!("preset:{}", preset),
        }
    }

    /// Checks if a cached image entry exists and is valid.
    ///
    /// For Dockerfile images, validates the hash. For preset images, just checks existence.
    ///
    /// # Arguments
    ///
    /// * `cache` - The image cache to check
    /// * `cache_key` - The cache key to look up
    ///
    /// # Returns
    ///
    /// `Ok(Some(entry))` if a valid cache entry exists, `Ok(None)` if no valid entry, or an error.
    ///
    /// # Errors
    ///
    /// Returns errors if the Dockerfile hash cannot be computed.
    fn check_cache_entry(
        &self,
        cache: &ImageCache,
        cache_key: &str,
    ) -> ProviderResult<Option<ImageCacheEntry>> {
        match &self.config.image_type {
            ModalImageType::Dockerfile { dockerfile } => {
                // Compute current hash
                let dockerfile_path = self.cache_dir.join(dockerfile);
                let current_hash = compute_file_hash(&dockerfile_path).map_err(|e| {
                    ProviderError::CreateFailed(format!("Failed to compute Dockerfile hash: {}", e))
                })?;

                info!(
                    "Dockerfile hash for {}: {}",
                    dockerfile_path.display(),
                    current_hash
                );

                // Check cache with hash validation
                Ok(cache.get_for_dockerfile(cache_key, &current_hash).cloned())
            }
            ModalImageType::Preset { .. } => {
                // For presets, just check if we have an entry
                Ok(cache.get(cache_key).cloned())
            }
        }
    }

    /// Updates the image cache with a newly built image.
    ///
    /// # Arguments
    ///
    /// * `cache_key` - The cache key to store the image under
    /// * `image_id` - The Modal image ID that was built
    ///
    /// # Errors
    ///
    /// Returns errors if:
    /// - Dockerfile hash cannot be computed
    /// - Cache cannot be saved to disk
    async fn update_cache(&self, cache_key: &str, image_id: &str) -> ProviderResult<()> {
        let mut cache = self.cache.lock().await;

        let dockerfile_hash = match &self.config.image_type {
            ModalImageType::Dockerfile { dockerfile } => {
                let dockerfile_path = self.cache_dir.join(dockerfile);
                Some(compute_file_hash(&dockerfile_path).map_err(|e| {
                    ProviderError::CreateFailed(format!("Failed to compute Dockerfile hash: {}", e))
                })?)
            }
            ModalImageType::Preset { .. } => None,
        };

        let entry = ImageCacheEntry {
            image_id: image_id.to_string(),
            dockerfile_hash,
            created_at: chrono::Utc::now().to_rfc3339(),
            image_type: match &self.config.image_type {
                ModalImageType::Dockerfile { .. } => "dockerfile".to_string(),
                ModalImageType::Preset { preset } => preset.clone(),
            },
        };

        cache.insert(cache_key.to_string(), entry);

        // Save cache to disk
        cache.save(&self.cache_dir).map_err(|e| {
            ProviderError::CreateFailed(format!("Failed to save image cache: {}", e))
        })?;

        info!("Cached Modal image {}: {}", cache_key, image_id);

        Ok(())
    }

    /// Gets or builds the Modal image, using cache when possible.
    ///
    /// This method ensures that only one build happens even when multiple sandboxes
    /// are created concurrently with a cache miss. It uses a `OnceCell` to coordinate
    /// the build across threads.
    ///
    /// # Returns
    ///
    /// The Modal image ID (e.g., "im-abc123")
    ///
    /// # Errors
    ///
    /// Returns errors if:
    /// - Dockerfile hash cannot be computed
    /// - Python build command fails
    /// - Cache cannot be saved
    async fn get_or_build_image(&self) -> ProviderResult<String> {
        let cache_key = self.get_cache_key();

        // Quick path: check cache first
        {
            let cache = self.cache.lock().await;
            if let Some(entry) = self.check_cache_entry(&cache, &cache_key)? {
                info!(
                    "Using cached Modal image for {}: {}",
                    cache_key, entry.image_id
                );
                return Ok(entry.image_id.clone());
            }
        }

        // Cache miss - get or create a OnceCell for this build
        let build_cell = {
            let mut builds = self.image_builds.lock().await;
            builds
                .entry(cache_key.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        // Only one thread will actually build; others wait
        let image_id = build_cell
            .get_or_try_init(|| async {
                info!("Cache miss for {}, building image...", cache_key);
                let image_id = self.call_python_build(&self.config.image_type).await?;

                // Update cache
                self.update_cache(&cache_key, &image_id).await?;

                Ok::<String, ProviderError>(image_id)
            })
            .await?;

        Ok(image_id.clone())
    }

    /// Calls Python script to build a Modal image.
    ///
    /// # Arguments
    ///
    /// * `image_type` - The image type configuration
    ///
    /// # Returns
    ///
    /// The Modal image ID returned by the build command
    ///
    /// # Errors
    ///
    /// Returns errors if the Python script fails or returns invalid output
    async fn call_python_build(&self, image_type: &ModalImageType) -> ProviderResult<String> {
        let command = match image_type {
            ModalImageType::Dockerfile { dockerfile } => {
                format!("uv run @modal_sandbox.py build dockerfile {}", dockerfile)
            }
            ModalImageType::Preset { preset } => {
                format!("uv run @modal_sandbox.py build preset {}", preset)
            }
        };

        debug!("Building Modal image: {}", command);

        let result = self.connector.run(&command).await?;

        // Forward stderr (contains build progress)
        if !result.stderr.is_empty() {
            for line in result.stderr.lines() {
                info!("{}", line);
            }
        }

        if result.exit_code != 0 {
            return Err(ProviderError::CreateFailed(format!(
                "Image build failed: {}",
                result.stderr
            )));
        }

        let image_id = result.stdout.trim().to_string();
        if image_id.is_empty() {
            return Err(ProviderError::CreateFailed(
                "Image build returned empty image_id".to_string(),
            ));
        }

        info!("Built Modal image: {}", image_id);
        Ok(image_id)
    }

    /// Creates a Modal sandbox using an existing image ID.
    ///
    /// # Arguments
    ///
    /// * `image_id` - The Modal image ID to use
    /// * `copy_dirs` - Directories to copy into the sandbox (local_path, remote_path)
    ///
    /// # Returns
    ///
    /// The Modal sandbox ID
    ///
    /// # Errors
    ///
    /// Returns errors if the Python script fails or returns invalid output
    async fn call_python_create(
        &self,
        image_id: &str,
        copy_dirs: &[(PathBuf, PathBuf)],
    ) -> ProviderResult<String> {
        let mut command = format!("uv run @modal_sandbox.py create {}", image_id);

        for (local, remote) in copy_dirs {
            command.push_str(&format!(
                " --copy-dir={}:{}",
                local.display(),
                remote.display()
            ));
        }

        debug!("Creating Modal sandbox: {}", command);

        let result = self.connector.run(&command).await?;

        // Forward stderr (contains creation progress)
        if !result.stderr.is_empty() {
            for line in result.stderr.lines() {
                info!("{}", line);
            }
        }

        if result.exit_code != 0 {
            return Err(ProviderError::CreateFailed(format!(
                "Sandbox creation failed: {}",
                result.stderr
            )));
        }

        let sandbox_id = result.stdout.trim().to_string();
        if sandbox_id.is_empty() {
            return Err(ProviderError::CreateFailed(
                "Sandbox creation returned empty sandbox_id".to_string(),
            ));
        }

        info!("Created Modal sandbox: {}", sandbox_id);
        Ok(sandbox_id)
    }
}

#[async_trait]
impl SandboxProvider for ModalProvider {
    type Sandbox = ModalSandbox;

    async fn create_sandbox(&self, config: &SandboxConfig) -> ProviderResult<ModalSandbox> {
        info!("Creating Modal sandbox: {}", config.id);

        let image_id = self.get_or_build_image().await?;

        let remote_id = self
            .call_python_create(&image_id, &config.copy_dirs)
            .await?;

        let info = ModalSandboxInfo {
            remote_id: remote_id.clone(),
            status: SandboxStatus::Running,
            created_at: chrono::Utc::now(),
        };
        self.sandboxes.lock().await.insert(config.id.clone(), info);

        Ok(ModalSandbox {
            id: config.id.clone(),
            remote_id,
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
}

/// A Modal cloud sandbox instance.
///
/// Represents a running Modal sandbox that can execute commands.
/// The sandbox uses the Modal Python CLI for all operations.
pub struct ModalSandbox {
    /// Local sandbox ID
    id: String,
    /// Remote Modal sandbox ID
    remote_id: String,
    /// Connector for running Python commands
    connector: Arc<ShellConnector>,
}

impl ModalSandbox {
    /// Builds the exec command for running a command in the sandbox.
    fn build_exec_command(&self, cmd: &Command) -> String {
        // Build the inner command with properly escaped arguments
        let inner_cmd = std::iter::once(cmd.program.as_str())
            .chain(cmd.args.iter().map(|s| s.as_str()))
            .map(|a| shell_words::quote(a).into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        // Escape the entire command so it can be passed as a single argument
        let escaped_cmd = shell_words::quote(&inner_cmd);

        format!(
            "uv run @modal_sandbox.py exec {} {}",
            self.remote_id, escaped_cmd
        )
    }

    /// Builds the destroy command for terminating the sandbox.
    fn build_destroy_command(&self) -> String {
        format!("uv run @modal_sandbox.py destroy {}", self.remote_id)
    }
}

#[async_trait]
impl Sandbox for ModalSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec_stream(&self, cmd: &Command) -> ProviderResult<OutputStream> {
        let shell_cmd = self.build_exec_command(cmd);
        debug!("Streaming on Modal {}: {}", self.remote_id, shell_cmd);
        self.connector.run_stream(&shell_cmd).await
    }

    async fn upload(&self, _local: &Path, _remote: &Path) -> ProviderResult<()> {
        warn!("upload() not supported by ModalSandbox - files should be included in Modal image");
        Ok(())
    }

    async fn download(&self, _remote: &Path, _local: &Path) -> ProviderResult<()> {
        warn!("download() not supported by ModalSandbox");
        Ok(())
    }

    fn status(&self) -> SandboxStatus {
        SandboxStatus::Running
    }

    async fn terminate(&self) -> ProviderResult<()> {
        let shell_cmd = self.build_destroy_command();
        info!(
            "Terminating Modal sandbox {} (remote: {})",
            self.id, self.remote_id
        );

        let result = self.connector.run(&shell_cmd).await?;

        if result.exit_code != 0 {
            warn!("Destroy command failed: {}", result.stderr);
        }

        Ok(())
    }
}
