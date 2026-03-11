//! Configuration schema types for deserialization from TOML.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Root configuration structure for offload.
///
/// This struct represents the complete configuration loaded from a TOML file.
/// It contains all settings needed to run tests: core settings, provider
/// configuration, test framework configuration, and reporting options.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Core offload settings
    pub offload: OffloadConfig,

    /// Provider configuration determines where tests are run
    pub provider: ProviderConfig,

    /// Framework configuration specifying how tests are discovered and run
    pub framework: FrameworkConfig,

    /// Group configuration allows segmenting tests into named groups
    pub groups: HashMap<String, GroupConfig>,

    /// Report configuration for output generation (optional, has defaults).
    #[serde(default)]
    pub report: ReportConfig,
}

/// Core offload execution settings.
///
/// These settings control the fundamental behavior of test execution:
/// how many tests run in parallel, timeout limits, and retry behavior.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OffloadConfig {
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

    /// Working directory for test execution.
    ///
    /// If specified, tests will run in this directory. Otherwise,
    /// the current working directory is used.
    pub working_dir: Option<PathBuf>,

    /// Project root path on the remote sandbox.
    ///
    /// Exported as OFFLOAD_ROOT environment variable in the sandbox.
    /// Used by test frameworks to compute paths relative to the project root,
    /// ensuring JUnit XML classnames match the test IDs from discovery.
    pub sandbox_project_root: String,

    /// Optional command to run during image build, after cwd/copy-dirs are applied.
    #[serde(default)]
    pub sandbox_init_cmd: Option<String>,
}

fn default_max_parallel() -> usize {
    10
}

fn default_test_timeout() -> u64 {
    900 // 15 minutes
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
/// | `modal` | Modal sandboxes | Modal cloud execution with simplified config |
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

    /// Run tests on Modal sandboxes with simplified configuration.
    ///
    /// Uses the DefaultSandbox implementation internally but exposes
    /// high-level configuration options instead of raw command strings.
    Modal(ModalProviderConfig),
}

/// Configuration for the local process provider.
///
/// Tests run as child processes of offload on the local machine.
/// This is the simplest provider and requires no external dependencies.
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

/// Configuration for Modal sandbox provider.
///
/// This provider runs tests on Modal sandboxes using a simplified configuration.
/// Instead of specifying raw shell commands, you provide high-level options
/// and the provider generates the appropriate Modal CLI commands internally.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModalProviderConfig {
    /// Path to a Dockerfile for building the sandbox image.
    ///
    /// If provided, Modal will build an image from this Dockerfile.
    /// If not specified, a default Modal image is used.
    #[serde(default)]
    pub dockerfile: Option<String>,

    /// Whether to include the current working directory in the image.
    ///
    /// When enabled, the entire current working directory is copied
    /// into the sandbox image during preparation.
    ///
    /// Default: false
    #[serde(default)]
    pub include_cwd: bool,

    /// Environment variables to set for all test processes.
    ///
    /// These are merged with (and override) the current environment.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Directories to copy into the sandbox image.
    ///
    /// Each entry is a string in the format "local_path:remote_path".
    /// These directories are baked into the image during preparation,
    /// making sandbox creation faster.
    #[serde(default)]
    pub copy_dirs: Vec<String>,
}

/// Configuration for custom remote execution provider.
///
/// This provider uses shell commands to manage sandbox lifecycle, enabling
/// integration with any cloud provider or execution environment. You define
/// three commands: create, exec, and destroy.
///
/// # Command Protocol
///
/// - **prepare_command** (optional): Runs once on first sandbox creation, prints image ID to stdout
/// - **create_command**: Prints a unique sandbox ID to stdout (can use `{image_id}` placeholder)
/// - **exec_command**: Uses `{sandbox_id}` and `{command}` placeholders
/// - **destroy_command**: Uses `{sandbox_id}` placeholder
/// - **download_command** (optional): Uses `{sandbox_id}` and `{paths}` placeholders for file download
///
/// The exec command can optionally return JSON for structured results:
/// ```json
/// {"exit_code": 0, "stdout": "...", "stderr": "..."}
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultProviderConfig {
    /// Optional command to prepare an image before sandbox creation.
    ///
    /// If provided, this command runs once on first sandbox creation and
    /// must print an image ID to stdout. The image ID is then available
    /// as `{image_id}` placeholder in `create_command`.
    ///
    /// This is useful for building container images or preparing
    /// execution environments that can be reused across multiple sandboxes.
    #[serde(default)]
    pub prepare_command: Option<String>,

    /// Command to create a new sandbox instance.
    ///
    /// Must print a unique sandbox ID to stdout. This ID will be passed
    /// to exec and destroy commands via the `{sandbox_id}` placeholder.
    ///
    /// If `prepare_command` is specified, `{image_id}` will be substituted
    /// with the image ID returned by the prepare command.
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

    /// Optional command to download files from a sandbox.
    ///
    /// Available placeholders:
    /// - `{sandbox_id}`: The ID returned by create_command
    /// - `{paths}`: Space-separated list of path specifications in "remote:local" format
    ///
    /// Each path specification downloads the remote path to the local path.
    /// Both files and directories are supported.
    #[serde(default)]
    pub download_command: Option<String>,

    /// Local working directory for running the lifecycle commands.
    ///
    /// Useful when commands are scripts in a specific directory.
    pub working_dir: Option<PathBuf>,

    /// Timeout for remote command execution in seconds.
    ///
    /// Default: 3600 (1 hour)
    #[serde(default = "default_remote_timeout")]
    pub timeout_secs: u64,

    /// Directories to copy into the image during prepare.
    ///
    /// Each entry is a string in the format "local_path:remote_path".
    /// These directories are baked into the image during the prepare step,
    /// making sandbox creation faster.
    #[serde(default)]
    pub copy_dirs: Vec<String>,

    /// Environment variables to set for all test processes.
    ///
    /// These are merged with (and override) the current environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_remote_timeout() -> u64 {
    3600 // 1 hour
}

/// Configuration for a test group.
///
/// Groups allow segmenting tests for different retry behaviors or filtering.
/// The test framework is configured at the top level, not per-group.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GroupConfig {
    /// Number of times to retry failed tests in this group.
    ///
    /// Failed tests are retried up to this many times. If a test passes
    /// on retry, it's marked as "flaky".
    ///
    /// Default: 0 (no retries)
    #[serde(default = "default_retry_count")]
    pub retry_count: usize,

    /// Filter strings passed to test frameworks during discovery.
    ///
    /// These filters are appended to the test discovery command to narrow
    /// down which tests are included in the group. The format depends on
    /// the test framework being used.
    ///
    /// An empty string means no filtering.
    #[serde(default)]
    pub filters: String,
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

    /// Discover and run JavaScript/TypeScript tests with vitest.
    Vitest(VitestFrameworkConfig),
}

impl FrameworkConfig {
    /// Returns the test ID format string for this framework.
    ///
    /// The format string is used to construct test IDs from JUnit XML attributes.
    /// Available placeholders are `{name}` and `{classname}`.
    pub fn test_id_format(&self) -> &str {
        match self {
            FrameworkConfig::Pytest(config) => &config.test_id_format,
            FrameworkConfig::Cargo(config) => &config.test_id_format,
            FrameworkConfig::Default(config) => &config.test_id_format,
            FrameworkConfig::Vitest(config) => &config.test_id_format,
        }
    }
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

    /// Full command prefix for invoking pytest (e.g. `"uv run pytest"`).
    ///
    /// When set, replaces the legacy `python` + `extra_args` + `-m pytest` combination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Extra arguments appended only during test execution (not discovery).
    ///
    /// Only used when `command` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_args: Option<String>,

    /// Format string for constructing test IDs from JUnit XML attributes.
    ///
    /// Available placeholders:
    /// - `{name}` - the testcase name attribute
    /// - `{classname}` - the testcase classname attribute
    ///
    /// Default: `"{name}"` (pytest typically includes full path in name)
    #[serde(default = "default_pytest_test_id_format")]
    pub test_id_format: String,
}

fn default_test_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("tests")]
}

fn default_python() -> String {
    "python".to_string()
}

fn default_pytest_test_id_format() -> String {
    "{name}".to_string()
}

fn default_cargo_test_id_format() -> String {
    "{classname} {name}".to_string()
}

fn default_vitest_command() -> String {
    "npx vitest".to_string()
}

fn default_vitest_test_id_format() -> String {
    "{classname} > {name}".to_string()
}

/// Configuration for vitest test framework.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VitestFrameworkConfig {
    /// Full command prefix for invoking vitest (e.g. `"npx vitest"`).
    ///
    /// Default: `"npx vitest"`
    #[serde(default = "default_vitest_command")]
    pub command: String,

    /// Extra arguments appended only during test execution (not discovery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_args: Option<String>,

    /// Format string for constructing test IDs from JUnit XML attributes.
    ///
    /// Default: `"{classname} > {name}"`
    #[serde(default = "default_vitest_test_id_format")]
    pub test_id_format: String,
}

impl Default for VitestFrameworkConfig {
    fn default() -> Self {
        Self {
            command: default_vitest_command(),
            run_args: None,
            test_id_format: default_vitest_test_id_format(),
        }
    }
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
    /// Maps to `cargo nextest run --run-ignored only`.
    ///
    /// Default: false
    #[serde(default)]
    pub include_ignored: bool,

    /// Format string for constructing test IDs from JUnit XML attributes.
    ///
    /// Available placeholders:
    /// - `{name}` - the testcase name attribute
    /// - `{classname}` - the testcase classname attribute
    ///
    /// Default: `"{classname} {name}"` (cargo/nextest uses classname as binary name)
    #[serde(default = "default_cargo_test_id_format")]
    pub test_id_format: String,
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
/// - **test_id_format**: Required format string for constructing test IDs from JUnit XML
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
    pub run_command: String,

    /// Path to JUnit XML result file produced by the test runner.
    ///
    /// If specified, offload will parse this file for detailed test results.
    /// Without this, results are inferred from exit codes only.
    pub result_file: Option<PathBuf>,

    /// Working directory for running test commands.
    pub working_dir: Option<PathBuf>,

    /// Format string for constructing test IDs from JUnit XML attributes.
    ///
    /// Available placeholders:
    /// - `{name}` - the testcase name attribute
    /// - `{classname}` - the testcase classname attribute
    ///
    /// This field is required for the default framework as the format varies
    /// by test runner.
    pub test_id_format: String,
}

/// Configuration for test result reporting.
///
/// Controls where test results are written and output format.
///
/// # Defaults
///
/// | Field | Default |
/// |-------|---------|
/// | `output_dir` | `"test-results"` |
/// | `junit` | `true` |
/// | `junit_file` | `"junit.xml"` |
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReportConfig {
    /// Directory where report files are written.
    ///
    /// Created automatically if it doesn't exist.
    ///
    /// Default: `"test-results"`
    #[serde(default = "default_report_dir")]
    pub output_dir: PathBuf,

    /// Enable JUnit XML output generation.
    ///
    /// When enabled, a JUnit XML file is written to `output_dir/junit_file`
    /// after tests complete. This is the primary result artifact for CI systems.
    ///
    /// Default: `true`
    #[serde(default = "default_junit")]
    pub junit: bool,

    /// Filename for JUnit XML output.
    ///
    /// Written to `output_dir/junit_file` when `junit` is enabled.
    ///
    /// Default: `"junit.xml"`
    #[serde(default = "default_junit_file")]
    pub junit_file: String,
}

fn default_report_dir() -> PathBuf {
    PathBuf::from("test-results")
}

fn default_junit() -> bool {
    true
}

fn default_junit_file() -> String {
    "junit.xml".to_string()
}

fn default_retry_count() -> usize {
    0
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

    /// Directories to copy to the sandbox.
    ///
    /// Each tuple is (local_path, remote_path).
    pub copy_dirs: Vec<(std::path::PathBuf, std::path::PathBuf)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modal_provider_with_dockerfile() -> Result<(), Box<dyn std::error::Error>> {
        let toml = r#"
            [offload]
            max_parallel = 4
            sandbox_project_root = "/app"

            [provider]
            type = "modal"
            dockerfile = ".devcontainer/Dockerfile"

            [framework]
            type = "pytest"
            test_id_format = "{classname}::{name}"

            [groups.all]
            retry_count = 1
        "#;

        let config: Config = toml::from_str(toml)?;

        assert!(
            matches!(&config.provider, ProviderConfig::Modal(_)),
            "Expected Modal provider"
        );

        if let ProviderConfig::Modal(modal_config) = &config.provider {
            assert_eq!(
                modal_config.dockerfile.as_deref(),
                Some(".devcontainer/Dockerfile")
            );
        }

        // Verify framework is at top level with test_id_format
        assert!(
            matches!(&config.framework, FrameworkConfig::Pytest(_)),
            "Expected Pytest framework"
        );
        assert_eq!(config.framework.test_id_format(), "{classname}::{name}");

        // Verify group only has retry_count
        assert_eq!(
            config
                .groups
                .get("all")
                .ok_or_else(|| anyhow::anyhow!("missing 'all' group"))?
                .retry_count,
            1
        );

        Ok(())
    }

    fn pytest_local_config() -> Config {
        Config {
            offload: OffloadConfig {
                max_parallel: 10,
                test_timeout_secs: 900,
                working_dir: None,
                sandbox_project_root: "/app".to_string(),
                sandbox_init_cmd: None,
            },
            provider: ProviderConfig::Local(LocalProviderConfig {
                working_dir: Some(PathBuf::from(".")),
                ..Default::default()
            }),
            framework: FrameworkConfig::Pytest(PytestFrameworkConfig {
                paths: vec![PathBuf::from("tests")],
                python: "python".into(),
                test_id_format: "{name}".into(),
                ..Default::default()
            }),
            groups: HashMap::from([(
                "default".to_string(),
                GroupConfig {
                    retry_count: 0,
                    filters: String::new(),
                },
            )]),
            report: ReportConfig::default(),
        }
    }

    fn cargo_local_config() -> Config {
        Config {
            offload: OffloadConfig {
                max_parallel: 10,
                test_timeout_secs: 900,
                working_dir: None,
                sandbox_project_root: "/app".to_string(),
                sandbox_init_cmd: None,
            },
            provider: ProviderConfig::Local(LocalProviderConfig {
                working_dir: Some(PathBuf::from(".")),
                ..Default::default()
            }),
            framework: FrameworkConfig::Cargo(CargoFrameworkConfig {
                test_id_format: "{classname} {name}".into(),
                ..Default::default()
            }),
            groups: HashMap::from([(
                "default".to_string(),
                GroupConfig {
                    retry_count: 0,
                    filters: String::new(),
                },
            )]),
            report: ReportConfig::default(),
        }
    }

    fn default_local_config() -> Config {
        Config {
            offload: OffloadConfig {
                max_parallel: 10,
                test_timeout_secs: 900,
                working_dir: None,
                sandbox_project_root: "/app".to_string(),
                sandbox_init_cmd: None,
            },
            provider: ProviderConfig::Local(LocalProviderConfig {
                working_dir: Some(PathBuf::from(".")),
                ..Default::default()
            }),
            framework: FrameworkConfig::Default(DefaultFrameworkConfig {
                discover_command: "echo test1 test2 {filters}".into(),
                run_command: "echo Running {tests}".into(),
                test_id_format: "{name}".into(),
                result_file: None,
                working_dir: None,
            }),
            groups: HashMap::from([(
                "default".to_string(),
                GroupConfig {
                    retry_count: 0,
                    filters: String::new(),
                },
            )]),
            report: ReportConfig::default(),
        }
    }

    /// Test that the Config built for pytest/local serializes to TOML and
    /// round-trips back through deserialization successfully.
    #[test]
    fn test_init_config_pytest_deserializes() -> Result<(), Box<dyn std::error::Error>> {
        let config = pytest_local_config();
        let toml_str = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_str)?;
        assert_eq!(deserialized.framework.test_id_format(), "{name}");
        Ok(())
    }

    /// Test that the Config built for cargo/local serializes to TOML and
    /// round-trips back through deserialization successfully.
    #[test]
    fn test_init_config_cargo_deserializes() -> Result<(), Box<dyn std::error::Error>> {
        let config = cargo_local_config();
        let toml_str = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_str)?;
        assert_eq!(
            deserialized.framework.test_id_format(),
            "{classname} {name}"
        );
        Ok(())
    }

    /// Test that the Config built for default/local serializes to TOML and
    /// round-trips back through deserialization successfully.
    #[test]
    fn test_init_config_default_deserializes() -> Result<(), Box<dyn std::error::Error>> {
        let config = default_local_config();
        let toml_str = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_str)?;
        assert_eq!(deserialized.framework.test_id_format(), "{name}");
        Ok(())
    }

    /// Test that sandbox_init_cmd deserializes from TOML and survives a round-trip.
    #[test]
    fn test_sandbox_init_cmd_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let toml_str = r#"
            [offload]
            sandbox_project_root = "/app"
            sandbox_init_cmd = "git apply /offload-upload/patch --allow-empty && uv sync --all-packages"

            [provider]
            type = "local"

            [framework]
            type = "cargo"

            [groups.all]
            retry_count = 0
        "#;

        let config: Config = toml::from_str(toml_str)?;
        assert_eq!(
            config.offload.sandbox_init_cmd.as_deref(),
            Some("git apply /offload-upload/patch --allow-empty && uv sync --all-packages")
        );

        let serialized = toml::to_string_pretty(&config)?;
        let round_tripped: Config = toml::from_str(&serialized)?;
        assert_eq!(
            round_tripped.offload.sandbox_init_cmd.as_deref(),
            Some("git apply /offload-upload/patch --allow-empty && uv sync --all-packages")
        );

        Ok(())
    }

    /// Test that `command` and `run_args` fields round-trip through TOML serialization.
    #[test]
    fn test_pytest_command_and_run_args_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let toml_str = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "pytest"
            command = "uv run pytest"
            run_args = "--no-cov"

            [groups.all]
            retry_count = 0
        "#;

        let config: Config = toml::from_str(toml_str)?;

        if let FrameworkConfig::Pytest(ref pytest) = config.framework {
            assert_eq!(pytest.command.as_deref(), Some("uv run pytest"));
            assert_eq!(pytest.run_args.as_deref(), Some("--no-cov"));
        } else {
            return Err("Expected Pytest framework".into());
        }

        let serialized = toml::to_string_pretty(&config)?;
        let round_tripped: Config = toml::from_str(&serialized)?;

        if let FrameworkConfig::Pytest(ref pytest) = round_tripped.framework {
            assert_eq!(pytest.command.as_deref(), Some("uv run pytest"));
            assert_eq!(pytest.run_args.as_deref(), Some("--no-cov"));
        } else {
            return Err("Expected Pytest framework after round-trip".into());
        }

        Ok(())
    }

    /// Test that configs without `command` / `run_args` still deserialize correctly.
    #[test]
    fn test_pytest_command_backward_compat() -> Result<(), Box<dyn std::error::Error>> {
        let toml_str = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "pytest"
            python = "python"
            extra_args = ["--timeout=60"]

            [groups.all]
            retry_count = 0
        "#;

        let config: Config = toml::from_str(toml_str)?;

        if let FrameworkConfig::Pytest(ref pytest) = config.framework {
            assert!(pytest.command.is_none());
            assert!(pytest.run_args.is_none());
            assert_eq!(pytest.python, "python");
            assert_eq!(pytest.extra_args, vec!["--timeout=60".to_string()]);
        } else {
            return Err("Expected Pytest framework".into());
        }

        Ok(())
    }

    fn vitest_local_config() -> Config {
        Config {
            offload: OffloadConfig {
                max_parallel: 10,
                test_timeout_secs: 900,
                working_dir: None,
                sandbox_project_root: "/app".to_string(),
                sandbox_init_cmd: None,
            },
            provider: ProviderConfig::Local(LocalProviderConfig {
                working_dir: Some(PathBuf::from(".")),
                ..Default::default()
            }),
            framework: FrameworkConfig::Vitest(VitestFrameworkConfig {
                command: "npx vitest".into(),
                test_id_format: "{classname} > {name}".into(),
                ..Default::default()
            }),
            groups: HashMap::from([(
                "default".to_string(),
                GroupConfig {
                    retry_count: 0,
                    filters: String::new(),
                },
            )]),
            report: ReportConfig::default(),
        }
    }

    #[test]
    fn test_init_config_vitest_deserializes() -> Result<(), Box<dyn std::error::Error>> {
        let config = vitest_local_config();
        let toml_str = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_str)?;
        assert_eq!(
            deserialized.framework.test_id_format(),
            "{classname} > {name}"
        );
        Ok(())
    }

    #[test]
    fn test_vitest_default_command() -> Result<(), Box<dyn std::error::Error>> {
        let toml_str = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "vitest"

            [groups.all]
            retry_count = 0
        "#;
        let config: Config = toml::from_str(toml_str)?;
        if let FrameworkConfig::Vitest(ref vitest) = config.framework {
            assert_eq!(vitest.command, "npx vitest");
            assert!(vitest.run_args.is_none());
        } else {
            return Err("Expected Vitest framework".into());
        }
        Ok(())
    }
}
