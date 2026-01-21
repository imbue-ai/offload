//! Configuration schema definitions.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Root configuration structure.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Core shotgun settings.
    pub shotgun: ShotgunConfig,

    /// Provider configuration.
    pub provider: ProviderConfig,

    /// Test discovery configuration.
    pub discovery: DiscoveryConfig,

    /// Report configuration (optional).
    #[serde(default)]
    pub report: ReportConfig,
}

/// Core shotgun settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShotgunConfig {
    /// Maximum number of parallel sandboxes.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,

    /// Test timeout in seconds.
    #[serde(default = "default_test_timeout")]
    pub test_timeout_secs: u64,

    /// Number of retries for failed tests.
    #[serde(default = "default_retry_count")]
    pub retry_count: usize,

    /// Working directory for test execution.
    pub working_dir: Option<PathBuf>,

    /// Stream test output in real-time instead of buffering.
    #[serde(default)]
    pub stream_output: bool,
}

fn default_max_parallel() -> usize {
    10
}

fn default_test_timeout() -> u64 {
    900 // 15 minutes
}

fn default_retry_count() -> usize {
    3
}

/// Provider configuration (tagged union).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    /// Local process provider.
    Process(ProcessProviderConfig),

    /// Docker container provider.
    Docker(DockerProviderConfig),

    /// SSH provider for remote machines (EC2, GCP, bare metal).
    Ssh(SshProviderConfig),

    /// Remote execution provider - plug in your own executor script (Modal, etc.).
    Remote(RemoteProviderConfig),
}

/// Configuration for the process provider.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProcessProviderConfig {
    /// Working directory for processes.
    pub working_dir: Option<PathBuf>,

    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Shell to use for running commands.
    #[serde(default = "default_shell")]
    pub shell: String,
}

fn default_shell() -> String {
    "/bin/sh".to_string()
}

/// Configuration for the Docker provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DockerProviderConfig {
    /// Docker image to use.
    pub image: String,

    /// Volume mounts (host:container format).
    #[serde(default)]
    pub volumes: Vec<String>,

    /// Environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory inside the container.
    pub working_dir: Option<String>,

    /// Network mode (bridge, host, none).
    #[serde(default = "default_network_mode")]
    pub network_mode: String,

    /// Docker host URL (defaults to local socket).
    pub docker_host: Option<String>,

    /// Resource limits.
    #[serde(default)]
    pub resources: DockerResourceConfig,
}

fn default_network_mode() -> String {
    "bridge".to_string()
}

/// Docker resource limits.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DockerResourceConfig {
    /// CPU limit (e.g., 2.0 for 2 CPUs).
    pub cpu_limit: Option<f64>,

    /// Memory limit in bytes.
    pub memory_limit: Option<i64>,

    /// Memory swap limit in bytes.
    pub memory_swap: Option<i64>,
}

/// Configuration for the SSH provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SshProviderConfig {
    /// List of hosts to connect to.
    pub hosts: Vec<String>,

    /// SSH username.
    pub user: String,

    /// Path to SSH private key.
    pub key_path: Option<PathBuf>,

    /// SSH port (defaults to 22).
    #[serde(default = "default_ssh_port")]
    pub port: u16,

    /// Working directory on remote hosts.
    pub working_dir: Option<String>,

    /// Known hosts file path.
    pub known_hosts: Option<PathBuf>,

    /// Whether to disable host key checking (not recommended for production).
    #[serde(default)]
    pub disable_host_key_check: bool,
}

fn default_ssh_port() -> u16 {
    22
}

/// Configuration for the remote execution provider.
///
/// Uses lifecycle-based execution: create sandbox, execute commands, destroy.
/// Your scripts handle Modal/EC2/Fly/etc.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteProviderConfig {
    /// Command to create a sandbox instance.
    /// Should print sandbox_id to stdout.
    pub create_command: String,

    /// Command to execute on a sandbox.
    /// Use {sandbox_id} and {command} as placeholders.
    /// Should output JSON: {"exit_code": 0, "stdout": "...", "stderr": "..."}
    pub exec_command: String,

    /// Command to destroy a sandbox.
    /// Use {sandbox_id} as placeholder.
    pub destroy_command: String,

    /// Working directory for running commands locally.
    pub working_dir: Option<PathBuf>,

    /// Timeout in seconds for remote execution.
    #[serde(default = "default_remote_timeout")]
    pub timeout_secs: u64,
}

fn default_remote_timeout() -> u64 {
    3600 // 1 hour
}

/// Test discovery configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DiscoveryConfig {
    /// pytest discovery.
    Pytest(PytestDiscoveryConfig),

    /// cargo test discovery.
    Cargo(CargoDiscoveryConfig),

    /// Generic/custom discovery.
    Generic(GenericDiscoveryConfig),
}

/// pytest discovery configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PytestDiscoveryConfig {
    /// Paths to search for tests.
    #[serde(default = "default_test_paths")]
    pub paths: Vec<PathBuf>,

    /// pytest markers to filter tests (e.g., "not slow").
    pub markers: Option<String>,

    /// Additional pytest arguments for collection.
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Python executable to use.
    #[serde(default = "default_python")]
    pub python: String,
}

fn default_test_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("tests")]
}

fn default_python() -> String {
    "python".to_string()
}

/// cargo test discovery configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CargoDiscoveryConfig {
    /// Package to test (for workspaces).
    pub package: Option<String>,

    /// Features to enable.
    #[serde(default)]
    pub features: Vec<String>,

    /// Test binary name (for --bin).
    pub bin: Option<String>,

    /// Whether to include ignored tests.
    #[serde(default)]
    pub include_ignored: bool,
}

/// Generic discovery configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenericDiscoveryConfig {
    /// Command to discover tests (should output test names, one per line).
    pub discover_command: String,

    /// Command template to run tests ({tests} will be replaced with test names).
    pub run_command: String,

    /// Path to result file (JUnit XML format).
    pub result_file: Option<PathBuf>,

    /// Working directory for commands.
    pub working_dir: Option<PathBuf>,
}

/// Report configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReportConfig {
    /// Output directory for reports.
    #[serde(default = "default_report_dir")]
    pub output_dir: PathBuf,

    /// Whether to generate JUnit XML.
    #[serde(default = "default_true")]
    pub junit: bool,

    /// JUnit XML filename.
    #[serde(default = "default_junit_file")]
    pub junit_file: String,
}

fn default_report_dir() -> PathBuf {
    PathBuf::from("test-results")
}

fn default_true() -> bool {
    true
}

fn default_junit_file() -> String {
    "junit.xml".to_string()
}

/// Configuration passed to sandbox creation.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Unique identifier for this sandbox.
    pub id: String,

    /// Working directory inside the sandbox.
    pub working_dir: Option<String>,

    /// Environment variables to set.
    pub env: Vec<(String, String)>,

    /// Resource limits.
    pub resources: SandboxResources,
}

/// Resource limits for a sandbox.
#[derive(Debug, Clone, Default)]
pub struct SandboxResources {
    /// CPU cores (e.g., 4.0 for 4 cores).
    pub cpu: Option<f64>,

    /// Memory in bytes.
    pub memory: Option<u64>,

    /// Timeout in seconds.
    pub timeout_secs: Option<u64>,
}
