//! Configuration schema definitions for shotgun.
//!
//! This module defines all configuration types that can be deserialized from
//! TOML configuration files. The schema uses serde for serialization and
//! tagged enums for provider/framework type selection.
//!
//! # Schema Overview
//!
//! ```text
//! Config (root)
//! ├── ShotgunConfig          - Core settings (parallelism, timeouts, retries)
//! ├── ProviderConfig         - Tagged enum selecting provider type
//! │   ├── Local              - Local process execution
//! │   └── Default            - Custom remote execution (Modal, etc.)
//! ├── FrameworkConfig        - Tagged enum selecting framework type
//! │   ├── Pytest             - pytest test framework
//! │   ├── Cargo              - Rust/Cargo test framework
//! │   └── Default            - Custom shell-based framework
//! └── ReportConfig           - Output and reporting settings
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Root configuration structure for shotgun.
///
/// This struct represents the complete configuration loaded from a TOML file.
/// It contains all settings needed to run tests: core settings, provider
/// configuration, test framework configuration, and reporting options.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Core shotgun settings
    pub shotgun: ShotgunConfig,

    /// Provider configuration determines where tests are run
    pub provider: ProviderConfig,

    /// Group configuration allows segmenting tests into named groups
    pub groups: HashMap<String, GroupConfig>,

    /// Report configuration for output generation (optional, has defaults).
    #[serde(default)]
    pub report: ReportConfig,
}

/// Core shotgun execution settings.
///
/// These settings control the fundamental behavior of test execution:
/// how many tests run in parallel, timeout limits, and retry behavior.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShotgunConfig {
    /// Maximum number of sandboxes to run in parallel.
    ///
    /// Higher values increase throughput but require more resources.
    /// Each sandbox may correspond to a local process or a ephemeral
    /// compute resource depending on the provider.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,

    /// Timeout for test execution in seconds.
    ///
    /// If a test batch takes longer than this, it will be terminated.
    /// Set this high enough for your slowest tests but low enough to
    /// catch hung tests.
    #[serde(default = "default_test_timeout")]
    pub test_timeout_secs: u64,

    /// Number of times to retry failed tests.
    ///
    /// Failed tests are retried up to this many times. If a test passes
    /// on retry, it's marked as "flaky". Set to 0 to disable retries.
    #[serde(default = "default_retry_count")]
    pub retry_count: usize,

    /// Working directory for test execution.
    ///
    /// If specified, tests will run in this directory. Otherwise,
    /// the current working directory is used.
    pub working_dir: Option<PathBuf>,

    /// Stream test output in real-time instead of buffering.
    ///
    /// When enabled, test output is printed as it occurs. When disabled
    /// (default), output is collected and displayed after each test completes.
    /// Streaming is useful for debugging but may interleave output from
    /// parallel tests.
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

/// Provider configuration specifying where tests run.
///
/// This is a tagged enum that selects the execution provider based on the
/// `type` field in TOML. Each variant contains provider-specific settings.
///
/// # Provider Types
///
/// | Type | Description | Use Case |
/// |------|-------------|----------|
/// | `local` | Local processes | Development, CI without containers |
/// | `default` | Custom shell commands | Cloud providers (Modal, Lambda, etc.) |
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    /// Run tests as local processes.
    ///
    /// The simplest provider - tests run directly on the host machine.
    /// Useful for development and CI environments without containerization.
    Local(LocalProviderConfig),

    /// Run tests using custom shell commands.
    ///
    /// Defines create/exec/destroy commands for lifecycle management.
    /// Use this to integrate with cloud providers like Modal, AWS Lambda,
    /// or any custom execution environment.
    Default(DefaultProviderConfig),
}

/// Configuration for the local process provider.
///
/// Tests run as child processes of shotgun on the local machine.
/// This is the simplest provider and requires no external dependencies.
///
/// # Example
///
/// ```toml
/// [provider]
/// type = "local"
/// working_dir = "/path/to/project"
/// shell = "/bin/bash"
///
/// [provider.env]
/// PYTHONPATH = "/app"
/// DEBUG = "1"
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LocalProviderConfig {
    /// Working directory for spawned processes.
    ///
    /// If not specified, uses the current working directory.
    pub working_dir: Option<PathBuf>,

    /// Environment variables to set for all test processes.
    ///
    /// These are merged with (and override) the current environment.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Shell to use for running commands.
    ///
    /// Commands are executed via `{shell} -c "{command}"`.
    ///
    /// Default: `/bin/sh`
    #[serde(default = "default_shell")]
    pub shell: String,
}

fn default_shell() -> String {
    "/bin/sh".to_string()
}

/// Configuration for custom remote execution provider.
///
/// This provider uses shell commands to manage sandbox lifecycle, enabling
/// integration with any cloud provider or execution environment. You define
/// three commands: create, exec, and destroy.
///
/// # Command Protocol
///
/// - **create_command**: Prints a unique sandbox ID to stdout
/// - **exec_command**: Uses `{sandbox_id}` and `{command}` placeholders
/// - **destroy_command**: Uses `{sandbox_id}` placeholder
///
/// The exec command can optionally return JSON for structured results:
/// ```json
/// {"exit_code": 0, "stdout": "...", "stderr": "..."}
/// ```
///
/// # Example: Modal Integration
///
/// ```toml
/// [provider]
/// type = "default"
/// create_command = "modal sandbox create --image python:3.11"
/// exec_command = "modal sandbox exec {sandbox_id} -- sh -c {command}"
/// destroy_command = "modal sandbox delete {sandbox_id}"
/// timeout_secs = 3600
/// ```
///
/// # Example: Custom Script
///
/// ```toml
/// [provider]
/// type = "default"
/// working_dir = "/path/to/scripts"
/// create_command = "./create_worker.sh"
/// exec_command = "./run_on_worker.sh {sandbox_id} {command}"
/// destroy_command = "./destroy_worker.sh {sandbox_id}"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultProviderConfig {
    /// Command to create a new sandbox instance.
    ///
    /// Must print a unique sandbox ID to stdout. This ID will be passed
    /// to exec and destroy commands via the `{sandbox_id}` placeholder.
    ///
    /// # Example
    /// ```sh
    /// # Simple: UUID generation
    /// uuidgen
    ///
    /// # Cloud: Create and return instance ID
    /// aws ec2 run-instances --query 'Instances[0].InstanceId' --output text
    /// ```
    pub create_command: String,

    /// Command to execute a test command on a sandbox.
    ///
    /// Available placeholders:
    /// - `{sandbox_id}`: The ID returned by create_command
    /// - `{command}`: The shell-escaped test command to run
    ///
    /// Can return plain text or JSON: `{"exit_code": N, "stdout": "...", "stderr": "..."}`
    pub exec_command: String,

    /// Command to destroy/cleanup a sandbox.
    ///
    /// Available placeholders:
    /// - `{sandbox_id}`: The ID returned by create_command
    ///
    /// Called after tests complete to release resources.
    pub destroy_command: String,

    /// Local working directory for running the lifecycle commands.
    ///
    /// Useful when commands are scripts in a specific directory.
    pub working_dir: Option<PathBuf>,

    /// Timeout for remote command execution in seconds.
    ///
    /// Default: 3600 (1 hour)
    #[serde(default = "default_remote_timeout")]
    pub timeout_secs: u64,
}

fn default_remote_timeout() -> u64 {
    3600 // 1 hour
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GroupConfig {
    /// Framework configuration for this group.
    pub framework: FrameworkConfig,
}

/// Test framework configuration specifying how tests are found and run.
///
/// This is a tagged enum that selects the test framework based on the
/// `type` field in TOML. Each variant contains framework-specific settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FrameworkConfig {
    /// Discover and run Python tests with pytest.
    Pytest(PytestFrameworkConfig),

    /// Discover and run Rust tests with cargo test.
    Cargo(CargoFrameworkConfig),

    /// Discover and run tests with custom shell commands.
    Default(DefaultFrameworkConfig),
}

/// Configuration for pytest test framework.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PytestFrameworkConfig {
    /// Directories to search for tests, relative to the working directory.
    ///
    /// Default: `["tests"]`
    #[serde(default = "default_test_paths")]
    pub paths: Vec<PathBuf>,

    /// pytest marker expression to filter tests.
    pub markers: Option<String>,

    /// Additional pytest arguments for test discovery.
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Python interpreter to use.
    #[serde(default = "default_python")]
    pub python: String,
}

fn default_test_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("tests")]
}

fn default_python() -> String {
    "python".to_string()
}

/// Configuration for Rust/Cargo test framework.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CargoFrameworkConfig {
    /// Package to test in a Cargo workspace.
    ///
    /// Maps to `cargo test -p <package>`. If not specified, tests all packages.
    pub package: Option<String>,

    /// Cargo features to enable during testing.
    ///
    /// Maps to `cargo test --features <features>`.
    #[serde(default)]
    pub features: Vec<String>,

    /// Specific binary to test.
    ///
    /// Maps to `cargo test --bin <name>`. Useful for testing binary crates.
    pub bin: Option<String>,

    /// Include tests marked with `#[ignore]`.
    ///
    /// Maps to `cargo test -- --ignored`.
    ///
    /// Default: false
    #[serde(default)]
    pub include_ignored: bool,
}

/// Configuration for generic/custom test framework.
///
/// Use this framework for any test runner by providing shell commands
/// for test discovery and execution. Output parsing relies on JUnit XML or
/// exit codes.
///
/// # Protocol
///
/// - **discover_command**: Outputs one test ID per line to stdout
/// - **run_command**: Uses `{tests}` placeholder for space-separated test IDs
/// - **result_file**: Optional JUnit XML for detailed results
///
/// # Example: Jest
///
/// ```toml
/// [framework]
/// type = "default"
/// discover_command = "jest --listTests --json | jq -r '.[]'"
/// run_command = "jest {tests} --ci --reporters=jest-junit"
/// result_file = "junit.xml"
/// ```
///
/// # Example: Go tests
///
/// ```toml
/// [framework]
/// type = "default"
/// discover_command = "go test -list '.*' ./... 2>/dev/null | grep -v '^ok\\|^$'"
/// run_command = "go test -v -run '{tests}' ./..."
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultFrameworkConfig {
    /// Command to discover test IDs.
    ///
    /// Should output one test ID per line to stdout. Lines starting with `#`
    /// are ignored (treated as comments).
    ///
    /// Run via shell: `sh -c "{discover_command}"`
    pub discover_command: String,

    /// Command template to run tests.
    ///
    /// The placeholder `{tests}` is replaced with space-separated test IDs.
    ///
    /// # Example
    /// ```toml
    /// run_command = "npm test -- {tests}"
    /// # Becomes: npm test -- test1 test2 test3
    /// ```
    pub run_command: String,

    /// Path to JUnit XML result file produced by the test runner.
    ///
    /// If specified, shotgun will parse this file for detailed test results.
    /// Without this, results are inferred from exit codes only.
    pub result_file: Option<PathBuf>,

    /// Working directory for running test commands.
    pub working_dir: Option<PathBuf>,
}

/// Configuration for test result reporting.
///
/// Controls where and how test results are written. The primary output
/// format is JUnit XML, which is widely supported by CI systems.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `output_dir` | `"test-results"` |
/// | `junit` | `true` |
/// | `junit_file` | `"junit.xml"` |
///
/// # Example
///
/// ```toml
/// [report]
/// output_dir = "build/test-results"
/// junit = true
/// junit_file = "results.xml"
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReportConfig {
    /// Directory where report files are written.
    ///
    /// Created automatically if it doesn't exist.
    ///
    /// Default: `"test-results"`
    #[serde(default = "default_report_dir")]
    pub output_dir: PathBuf,

    /// Whether to generate JUnit XML report.
    ///
    /// JUnit XML is the standard format for CI/CD systems like
    /// Jenkins, GitLab CI, GitHub Actions, etc.
    ///
    /// Default: `true`
    #[serde(default = "default_true")]
    pub junit: bool,

    /// Filename for the JUnit XML report.
    ///
    /// Written to `{output_dir}/{junit_file}`.
    ///
    /// Default: `"junit.xml"`
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

/// Runtime configuration passed to sandbox creation.
///
/// This struct is used internally by the orchestrator to configure each
/// sandbox instance. It contains the runtime-specific settings derived
/// from the main configuration.
///
/// Unlike the TOML configuration structs, this is not serializable and
/// is constructed programmatically.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Unique identifier for this sandbox instance.
    ///
    /// Used for tracking, logging, and cleanup. Typically a UUID.
    pub id: String,

    /// Working directory inside the sandbox.
    ///
    /// Test commands will execute from this directory.
    pub working_dir: Option<String>,

    /// Environment variables to set in the sandbox.
    ///
    /// Passed as key-value tuples.
    pub env: Vec<(String, String)>,

    /// Resource limits for this sandbox.
    pub resources: SandboxResources,
}

/// Resource limits for a sandbox instance.
#[derive(Debug, Clone, Default)]
pub struct SandboxResources {
    /// Execution timeout in seconds.
    ///
    /// Commands exceeding this limit are terminated.
    pub timeout_secs: Option<u64>,
}
